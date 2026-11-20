import { Box, Button, ButtonGroup, Group, Text } from '@vkontakte/vkui';
import { formatCount, reasonRows, reportFailedText, reusedText, savedText, stopText, unrewrittenText } from '../run-state.js';
import { Notice, SCREEN_TITLE } from './common.jsx';

const ROW = { display: 'flex', gap: 12, justifyContent: 'space-between', padding: '6px 0' };

export default function Result({ s, onOpenBrowser, onOpenFolder, onResume, onPickOther, disabled }) {
  const v = s.view;
  const reasons = v ? reasonRows(v) : [];
  return (
    <Group>
      <Box padding="m">
        <Text style={SCREEN_TITLE}>Готово</Text>
        {v && <Text weight="2">{savedText(v)}</Text>}
      </Box>
      <Notice text={reusedText(s.report)} mode="default" />
      <Notice text={stopText(s.stop)} mode={s.stop === 'cancelled' ? 'default' : 'error'} />
      <Notice text={unrewrittenText(s.report)} mode="error" />
      <Notice text={reportFailedText(s.report)} mode="error" />
      {reasons.length > 0 && (
        <Box padding="m">
          <Text>Не скачано:</Text>
          <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
            {reasons.map((r) => (
              <li key={r.label} style={ROW}><Text>{r.label}</Text><Text>{formatCount(r.count)}</Text></li>
            ))}
          </ul>
        </Box>
      )}
      <Box padding="m">
        <ButtonGroup gap="m" style={{ flexWrap: 'wrap' }}>
          <Button size="l" onClick={onOpenBrowser}>Открыть архив в браузере</Button>
          <Button size="l" mode="secondary" onClick={onOpenFolder}>Открыть папку</Button>
          <Button size="l" mode="secondary" onClick={onResume} disabled={disabled}>Докачать</Button>
          <Button size="l" mode="secondary" onClick={onPickOther} disabled={disabled}>Выбрать другую папку</Button>
        </ButtonGroup>
      </Box>
    </Group>
  );
}
