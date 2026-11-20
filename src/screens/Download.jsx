import { Box, Button, Group, Progress, Spinner, Text } from '@vkontakte/vkui';
import { countsText, progressText } from '../run-state.js';
import { Notice, SCREEN_TITLE } from './common.jsx';

export default function Download({ s, onStop }) {
  const p = s.progress;
  const t = progressText(p);
  const counts = countsText(p);
  return (
    <Group>
      <Box padding="m">
        <Text style={SCREEN_TITLE}>Скачивание</Text>
        {p ? (
          <>
            <Progress value={p.total ? (100 * p.done) / p.total : 0} aria-label="Ход скачивания" />
            {counts && <Text style={{ marginTop: 8 }}>{counts}</Text>}
            <Text style={{ marginTop: 8 }}>{t.count + ' – ' + t.speed}</Text>
            {t.eta && <Text>{'Осталось: ' + t.eta}</Text>}
            {t.section && <Text>{t.section}</Text>}
          </>
        ) : (
          <div style={{ display: 'flex', gap: 12, alignItems: 'center' }} role="status" aria-live="polite">
            <Spinner size="s" />
            <Text>Подождите</Text>
          </div>
        )}
      </Box>
      <Notice text={s.pausedText} />
      <Box padding="m">
        <Button size="l" mode="secondary" onClick={onStop} disabled={s.stopping}>Остановить</Button>
      </Box>
    </Group>
  );
}
