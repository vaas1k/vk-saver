import { Box, Button, ButtonGroup, Group, Header, Text } from '@vkontakte/vkui';
import { formatCount, reasonRows, reportFailedText, reusedText, savedText, stopText, unrewrittenText } from '../run-state.js';
import { Notice } from './common.jsx';

const ROW = { display: 'flex', gap: 12, justifyContent: 'space-between', padding: '6px 0' };

export default function Result({ s, onOpenBrowser, onOpenFolder, onResume, onPickOther, onUndoAsk, onUndoCancel, onUndo, onSaveReport, disabled }) {
  const v = s.view;
  const reasons = v ? reasonRows(v) : [];
  return (
    <Group header={<Header>Готово</Header>}>
      {v && <Box padding="m"><Text weight="2">{savedText(v)}</Text></Box>}
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
          <Button size="l" mode="tertiary" onClick={onSaveReport} disabled={disabled}>Сохранить отчёт для разработчика</Button>
          <Button size="l" mode="tertiary" onClick={onUndoAsk} disabled={disabled || s.undoAsk}>Вернуть исходные страницы</Button>
        </ButtonGroup>
      </Box>
      {s.undoAsk && (
        <Box padding="m">
          <Text>Ссылки на страницах снова будут вести во ВКонтакте. Скачанные файлы останутся на диске.</Text>
          <ButtonGroup gap="m" style={{ marginTop: 8 }}>
            <Button size="m" onClick={onUndo} disabled={disabled}>Вернуть</Button>
            <Button size="m" mode="secondary" onClick={onUndoCancel} disabled={disabled}>Отмена</Button>
          </ButtonGroup>
        </Box>
      )}
      <Notice text={s.undoText} mode={s.undoError ? 'error' : 'default'} />
    </Group>
  );
}
