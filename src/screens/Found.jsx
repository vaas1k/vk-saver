import { Box, Button, ButtonGroup, Checkbox, Group, Header, SegmentedControl, Text } from '@vkontakte/vkui';
import { allSelected, chatRows, shownChats, etaText, formatBytes, foundRows, linksText, nothingToDownload, tooLargeText } from '../run-state.js';
import { Busy, LIST_ROW } from './common.jsx';

const LIST = { listStyle: 'none', margin: 0, padding: 0 };
const ROW = { display: 'flex', gap: 12, justifyContent: 'space-between', flexWrap: 'wrap', padding: '6px 0' };

export default function Found({ summary, excluded, chatKind, onChatKind, onToggle, onToggleAll, onStart, onPickOther, disabled }) {
  if (!summary) return <Group><Busy /></Group>;
  const chats = chatRows(summary);
  const shown = shownChats(chats, chatKind);
  const selected = shown.filter((c) => !excluded.includes(String(c.peer))).length;
  const links = linksText(summary);
  const eta = etaText(summary.eta_s);
  const tooLarge = tooLargeText(summary);
  const empty = nothingToDownload(summary);
  return (
    <Group header={<Header>Что найдено</Header>}>
      <Box padding="m">
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
          <details style={{ marginTop: 8 }}>
            <summary><Text Component="span">{'Чаты: ' + chats.length}</Text></summary>
            <SegmentedControl
              size="m"
              name="chat-kind"
              value={chatKind}
              onChange={onChatKind}
              options={[{ label: 'Все', value: 'all' }, { label: 'Личные', value: 'user' }, { label: 'Беседы', value: 'group' }]}
              style={{ margin: '8px 0 4px' }}
            />
            <Button size="s" mode="tertiary" onClick={onToggleAll} disabled={shown.length === 0} style={{ margin: '4px 0' }}>
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
          </details>
        )}
        {links && <Text style={{ marginTop: 8 }}>{links}</Text>}
        {eta && <Text style={{ marginTop: 8 }}>{'Время: ' + eta}</Text>}
        {tooLarge && <Text style={{ marginTop: 8 }}>{tooLarge}</Text>}
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
