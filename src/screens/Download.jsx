import { Box, Button, Group, Header, Progress, Text } from '@vkontakte/vkui';
import { countsText, progressText } from '../run-state.js';
import { Busy, Notice } from './common.jsx';

export default function Download({ s, onStop }) {
  const p = s.progress;
  const t = progressText(p);
  const counts = countsText(p);
  return (
    <Group header={<Header>Скачивание</Header>}>
      {p ? (
        <Box padding="m">
          <Progress value={p.total ? (100 * p.done) / p.total : 0} aria-label="Ход скачивания" />
          {counts && <Text style={{ marginTop: 8 }}>{counts}</Text>}
          <Text style={{ marginTop: 8 }}>{t.count + ' – ' + t.speed}</Text>
          {t.eta && <Text>{'Осталось: ' + t.eta}</Text>}
          {t.section && <Text>{t.section}</Text>}
        </Box>
      ) : <Busy />}
      <Notice text={s.pausedText} />
      <Box padding="m">
        <Button size="l" mode="secondary" onClick={onStop} disabled={s.stopping}>Остановить</Button>
      </Box>
    </Group>
  );
}
