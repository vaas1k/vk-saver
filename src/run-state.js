const UNITS = ['КБ', 'МБ', 'ГБ', 'ТБ'];

export function formatBytes(n) {
  if (n < 1024) return n + ' Б';
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024;
    i += 1;
  }
  return v.toFixed(1).replace('.', ',') + ' ' + UNITS[i];
}

export const formatCount = (n) => n.toLocaleString('ru-RU');

export const initial = {
  screen: 'pick',
  error: null,
  summary: null,
  excluded: [],
  chatKind: 'all',
  progress: null,
  pausedText: null,
  stopping: false,
  report: null,
  stop: null,
  view: null,
  unzip: null,
  undoAsk: false,
  undoText: null,
  undoError: false,
  notice: '',
  reportText: '',
};

const STOP_TEXTS = {
  throttled: 'ВКонтакте ограничил скачивание, попробуйте позже',
  network: 'Нет соединения с интернетом – нажмите "Докачать", когда оно появится',
  no_space: 'На диске закончилось место',
  cancelled: 'Остановлено. То, что уже скачано, сохранено.',
  not_export: 'Это не похоже на архив ВКонтакте. Запросите архив по ссылке на странице помощи ВКонтакте выше',
  already_open: 'vk-saver уже открыт',
  bad_zip: 'Этот ZIP повреждён или небезопасен – распакуйте его сами и выберите папку',
};

export const SAVE_REPORT_ERROR = 'Не получилось сохранить отчёт';
export const UNDO_ERROR = 'Не получилось вернуть страницы';

const ZIP_TEXTS = {
  storage: 'Не получилось распаковать рядом с архивом – распакуйте его сами и выберите папку',
  cancelled: 'Распаковка остановлена',
};

export function stopText(kind) {
  if (!kind) return '';
  return STOP_TEXTS[kind] ?? 'Что-то пошло не так, попробуйте ещё раз';
}

const pad = (n) => String(n).padStart(2, '0');

export function reduce(s, msg) {
  switch (msg.type) {
    case 'opened':
      if (!msg.reply) return s;
      if (msg.reply.error) {
        const error = (msg.zip && ZIP_TEXTS[msg.reply.error]) || stopText(msg.reply.error);
        return { ...s, screen: 'pick', error, unzip: null };
      }
      return { ...initial, screen: 'found' };
    case 'scanned':
      if (msg.reply.error) return { ...initial, error: stopText(msg.reply.error) };
      return { ...s, summary: msg.reply, chatKind: 'all' };
    case 'chat_kind':
      return { ...s, chatKind: msg.kind };
    case 'toggle': {
      const peer = String(msg.peer);
      const excluded = s.excluded.includes(peer) ? s.excluded.filter((p) => p !== peer) : [...s.excluded, peer];
      return { ...s, excluded };
    }
    case 'toggle_all': {
      const peers = shownChats(chatRows(s.summary), s.chatKind).map((c) => String(c.peer));
      const rest = s.excluded.filter((p) => !peers.includes(p));
      return { ...s, excluded: allSelected(peers, s.excluded) ? [...rest, ...peers] : rest };
    }
    case 'started':
      return { ...s, screen: 'download', progress: null, pausedText: null, stopping: false, report: null, stop: null, view: null, undoAsk: false, undoText: null, undoError: false, notice: '', reportText: '' };
    case 'progress':
      // byte-level events carry section null: keep the last known label
      return { ...s, progress: { ...msg.p, section: msg.p.section ?? s.progress?.section ?? null }, pausedText: null };
    case 'paused': {
      const d = new Date(msg.until_ms);
      return { ...s, pausedText: 'ВКонтакте просит подождать, продолжим в ' + pad(d.getHours()) + ':' + pad(d.getMinutes()) };
    }
    case 'unzip':
      return { ...s, error: null, unzip: { done: msg.p.done, total: msg.p.total } };
    case 'pick':
      return initial;
    case 'stopping':
      return { ...s, stopping: true };
    case 'finished':
      if (msg.reply.error === 'busy') return s;
      return { ...s, report: msg.reply, stop: msg.reply.stop ?? msg.reply.error ?? null, pausedText: null };
    case 'result':
      return { ...s, screen: 'result', view: msg.reply.error ? null : msg.reply };
    case 'undo_ask':
      return { ...s, undoAsk: true, undoText: null };
    case 'undo_cancel':
      return { ...s, undoAsk: false };
    case 'undone': {
      const error = msg.reply.error;
      // The undone pages are no longer "not updated": drop that notice with the saved-report text.
      const report = s.report && { ...s.report, rewrite: {} };
      const text = error ? (error === 'storage' ? UNDO_ERROR : stopText(error)) : undoText(msg.reply);
      return { ...s, report, notice: '', reportText: '', undoAsk: false, undoError: Boolean(error), undoText: text };
    }
    case 'notice':
      return { ...s, notice: msg.text };
    case 'report_saved':
      return { ...s, notice: msg.error ? (msg.error === 'storage' ? SAVE_REPORT_ERROR : stopText(msg.error)) : '', reportText: msg.error ? '' : 'Отчёт сохранён' };
    default:
      return s;
  }
}

export const unzipText = (u) => (u ? 'Распаковка: ' + formatBytes(u.done) + ' из ' + formatBytes(u.total) : '');

export function etaText(secs) {
  if (secs == null) return '';
  if (secs < 60) return 'меньше минуты';
  if (secs < 3600) return 'около ' + Math.round(secs / 60) + ' мин';
  return 'около ' + Math.round(secs / 3600) + ' ч';
}

const SECTIONS = { albums: 'Фотографии из альбомов', messages: 'Фото в переписках', profile: 'Профиль', other: 'Другое' };

export function progressText(p) {
  if (!p) return { count: '', speed: '', eta: '', section: '' };
  return {
    count: formatCount(p.done) + ' из ' + formatCount(p.total),
    speed: p.mb_s.toFixed(1).replace('.', ',') + ' МБ/с',
    eta: etaText(p.eta_s),
    section: SECTIONS[p.section] ?? '',
  };
}

export function countsText(p) {
  return [
    ['Сохранено', p?.saved],
    ['удалено во ВКонтакте', p?.gone],
    ['не скачано', p?.failed],
  ]
    .filter(([, n]) => n > 0)
    .map(([w, n]) => w + ' ' + formatCount(n))
    .join(' · ');
}

const groupRow = (label, g) => ({
  label,
  text: formatCount(g.count) + ', ~' + formatBytes(g.bytes),
  saved: g.saved > 0 ? 'уже скачано ' + formatCount(g.saved) : '',
});

// Empty groups are dropped: the scan does not fill every group yet.
export function foundRows(sum) {
  return [
    ['Фотографии из альбомов', sum.albums],
    ['Фото в переписках', sum.messages],
    ['Голосовые сообщения', sum.voice],
  ]
    .filter(([, g]) => g.count > 0)
    .map(([label, g]) => groupRow(label, g));
}

export const nothingToDownload = (sum) => foundRows(sum).length === 0 && chatRows(sum).length === 0;

// VK convention: group chats have peer ids from 2000000000; users and communities (negative) are personal
export const isGroupChat = (peer) => peer >= 2000000000;

export const shownChats = (rows, kind) => (kind === 'all' ? rows : rows.filter((c) => isGroupChat(c.peer) === (kind === 'group')));

export const allSelected = (peers, excluded) => peers.every((p) => !excluded.includes(String(p)));

export function chatRows(sum) {
  return [...sum.chats]
    .map((c) => ({ peer: c.peer, label: c.label || 'Чат ' + c.peer, bytes: c.photos.bytes + c.voice.bytes, count: c.photos.count + c.voice.count }))
    .filter((c) => c.count > 0)
    .sort((a, b) => b.bytes - a.bytes)
    .map((c) => ({ ...c, text: formatCount(c.count) + ', ~' + formatBytes(c.bytes) }));
}

export function linksText(sum) {
  const parts = [
    ['видео', sum.videos],
    ['документы', sum.docs],
    ['фото', sum.photo_links],
  ]
    .filter(([, n]) => n > 0)
    .map(([w, n]) => w + ' ' + formatCount(n));
  return parts.length ? 'Только ссылки на ВК (не скачиваются): ' + parts.join(', ') : '';
}

export function reasonRows(v) {
  return [
    ['удалены во ВКонтакте (404)', v.gone],
    ['старый сервер ВКонтакте не отвечает', v.host_dead],
    ['не удалось скачать – попробуйте ещё раз', v.failed],
    ['видео и документы – только ссылки', v.videos + v.docs + v.photo_links],
  ]
    .filter(([, n]) => n > 0)
    .map(([label, count]) => ({ label, count }));
}

function plural(n, one, few, many) {
  const m10 = n % 10;
  const m100 = n % 100;
  if (m10 === 1 && m100 !== 11) return one;
  if (m10 >= 2 && m10 <= 4 && (m100 < 12 || m100 > 14)) return few;
  return many;
}

const pages = (n) => formatCount(n) + ' ' + plural(n, 'страница', 'страницы', 'страниц');

export const tooLargeText = (sum) => (sum.too_large > 0 ? 'Слишком большие страницы пропущены: ' + formatCount(sum.too_large) : null);

export function undoText(r) {
  const kept = r.kept + (r.skipped ?? 0);
  const left = r.busy > 0 || kept > 0;
  const parts = r.restored > 0 || !left ? ['Страницы возвращены. Файлы остались в папках media – "Докачать" снова подключит их'] : [];
  if (r.busy > 0) parts.push(pages(r.busy) + ' ' + plural(r.busy, 'открыта', 'открыты', 'открыты') + ' в другой программе – закройте её и нажмите ещё раз');
  if (kept > 0) parts.push(pages(kept) + ' ' + plural(kept, 'изменена', 'изменены', 'изменены') + ' после vk-saver и ' + plural(kept, 'оставлена', 'оставлены', 'оставлены') + ' как есть');
  return parts.join('. ');
}

export const savedText = (v) => 'Скачано ' + formatCount(v.saved) + ' ' + plural(v.saved, 'файл', 'файла', 'файлов') + ', ' + formatBytes(v.bytes);

export const reusedText = (report) => (report?.reused > 0 ? 'Взято из прошлого архива: ' + formatCount(report.reused) : null);

export function unrewrittenText(report) {
  const n = (report?.rewrite?.skipped ?? 0) + (report?.rewrite?.busy ?? 0);
  return n > 0 ? 'Не удалось обновить страниц: ' + formatCount(n) : null;
}

export const reportFailedText = (report) => (report?.report_failed ? 'Не получилось записать report.html' : null);
