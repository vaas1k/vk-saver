import { Box, Button, ButtonGroup, Checkbox, Group, Input, SegmentedControl, Spinner, Text } from '@vkontakte/vkui';
import { useState } from 'react';
import * as api from '../api.js';
import { allSelected, chatRows, shownChats, etaText, formatBytes, foundRows, linksText, nothingToDownload, stopText, tooLargeText } from '../run-state.js';
import { Busy, LIST_ROW, SCREEN_TITLE } from './common.jsx';

const LIST = { listStyle: 'none', margin: 0, padding: 0 };
const ROW = { display: 'flex', gap: 12, justifyContent: 'space-between', flexWrap: 'wrap', padding: '6px 0' };
// Текст tertiary-кнопок на общей левой линии: у кнопки нет фона, внутренний отступ не нужен.
const FLAT = { paddingInlineStart: 0 };
const SORTS = [
  { label: 'Размер', value: 'size' },
  { label: 'Дата', value: 'date' },
  { label: 'Сообщения', value: 'messages' },
  { label: 'Название', value: 'name' },
];

export default function Found({ summary, excluded, chatKind, onChatKind, onToggle, onToggleAll, onStart, onPickOther, onOpenPage, disabled }) {
  const [chatsOpen, setChatsOpen] = useState(false);
  const [sort, setSort] = useState('size');
  const [q, setQ] = useState('');
  const [searching, setSearching] = useState(false);
  const [searched, setSearched] = useState(false);
  const [matches, setMatches] = useState([]);
  const [searchError, setSearchError] = useState(null);
  if (!summary) return <Group><Busy /></Group>;
  const chats = chatRows(summary, sort);
  const shown = shownChats(chats, chatKind);
  const selected = shown.filter((c) => !excluded.includes(String(c.peer))).length;
  const links = linksText(summary);
  const eta = etaText(summary.eta_s);
  const tooLarge = tooLargeText(summary);
  const empty = nothingToDownload(summary);
  const labels = new Map(summary.chats.map((c) => [String(c.peer), c.label || 'Чат ' + c.peer]));

  const runSearch = async () => {
    const query = q.trim();
    if (query.length < 2 || searching) return;
    setSearching(true);
    try {
      const r = await api.search(query);
      if (Array.isArray(r)) {
        setMatches(r);
        setSearchError(null);
      } else {
        setMatches([]);
        setSearchError(stopText(r?.error));
      }
    } catch {
      setMatches([]);
      setSearchError(null);
    } finally {
      setSearched(true);
      setSearching(false);
    }
  };

  return (
    <Group>
      <Box padding="m">
        <Text style={SCREEN_TITLE}>Что найдено</Text>
        <Text>{'Свободно: ' + formatBytes(summary.free_bytes)}</Text>
        <ul style={{ ...LIST, marginTop: 8 }}>
          {foundRows(summary).map((r) => (
            <li key={r.label} style={ROW}>
              <Text weight="2">{r.label}</Text>
              <Text>{r.text + (r.saved ? ' (' + r.saved + ')' : '')}</Text>
            </li>
          ))}
        </ul>
        {chats.length > 0 && (
          <>
            <Button size="s" mode="tertiary" style={{ ...FLAT, marginTop: 8 }} onClick={() => setChatsOpen((o) => !o)}>
              {'Чаты: ' + chats.length + (chatsOpen ? ' ▾' : ' ▸')}
            </Button>
            {chatsOpen && (
              <>
                <SegmentedControl
                  size="m"
                  name="chat-kind"
                  value={chatKind}
                  onChange={onChatKind}
                  options={[{ label: 'Все', value: 'all' }, { label: 'Личные', value: 'user' }, { label: 'Беседы', value: 'group' }]}
                  style={{ margin: '8px 0 4px' }}
                />
                <SegmentedControl
                  size="m"
                  name="chat-sort"
                  value={sort}
                  onChange={setSort}
                  options={SORTS}
                  style={{ margin: '0 0 4px' }}
                />
                <Button size="s" mode="tertiary" style={{ ...FLAT, margin: '4px 0' }} onClick={onToggleAll} disabled={shown.length === 0}>
                  {allSelected(shown.map((c) => c.peer), excluded) ? 'Снять выделение' : 'Выделить всё'}
                </Button>
                <Text style={{ margin: '4px 0' }}>{'Показано: ' + shown.length + ', выбрано: ' + selected}</Text>
                <ul style={LIST}>
                  {shown.map((c) => (
                    <li key={c.peer} style={LIST_ROW}>
                      <Checkbox checked={!excluded.includes(String(c.peer))} onChange={() => onToggle(c.peer)} description={c.text}>
                        {c.label}
                      </Checkbox>
                    </li>
                  ))}
                </ul>
              </>
            )}
          </>
        )}
        {links && <Text style={{ marginTop: 8 }}>{links}</Text>}
        {eta && <Text style={{ marginTop: 8 }}>{'Время: ' + eta}</Text>}
        {tooLarge && <Text style={{ marginTop: 8 }}>{tooLarge}</Text>}

        <Text style={{ marginTop: 16, fontWeight: 600 }}>Поиск по сообщениям</Text>
        <form onSubmit={(e) => { e.preventDefault(); runSearch(); }} style={{ display: 'flex', gap: 8, marginTop: 8 }}>
          <Input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Текст из переписок" aria-label="Поиск по сообщениям" disabled={disabled} />
          <Button size="m" type="submit" disabled={disabled || q.trim().length < 2 || searching}>Найти</Button>
        </form>
        {searching && (
          <div style={{ display: 'flex', gap: 12, alignItems: 'center', marginTop: 8 }} role="status" aria-live="polite">
            <Spinner size="s" />
            <Text>Ищем</Text>
          </div>
        )}
        {searchError && <Text style={{ marginTop: 8 }}>{searchError}</Text>}
        {searched && !searching && !searchError && matches.length === 0 && <Text style={{ marginTop: 8 }}>Ничего не найдено</Text>}
        {matches.length > 0 && (
          <ul style={{ ...LIST, marginTop: 8 }}>
            {matches.map((m, i) => (
              <li key={i} style={LIST_ROW}>
                <Button
                  mode="tertiary"
                  size="m"
                  stretched
                  style={{ ...FLAT, height: 'auto', whiteSpace: 'normal', textAlign: 'left', justifyContent: 'flex-start' }}
                  onClick={() => onOpenPage(m.page)}
                >
                  <span style={{ display: 'block' }}><Text weight="2">{labels.get(String(m.peer)) ?? 'Чат ' + m.peer}</Text></span>
                  <span style={{ display: 'block' }}><Text>{m.snippet}</Text></span>
                </Button>
              </li>
            ))}
          </ul>
        )}
      </Box>
      <Box padding="m">
        {empty && <Text style={{ marginBottom: 8 }}>В этом архиве нечего скачивать</Text>}
        <ButtonGroup gap="m" style={{ flexWrap: 'wrap' }}>
          <Button size="l" onClick={onStart} disabled={empty || disabled}>Скачать</Button>
          <Button size="l" mode="secondary" onClick={onPickOther} disabled={disabled}>Выбрать другую папку</Button>
        </ButtonGroup>
      </Box>
    </Group>
  );
}
