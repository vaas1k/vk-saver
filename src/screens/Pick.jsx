import { Box, Button, ButtonGroup, Group, Link, Spinner, Text } from '@vkontakte/vkui';
import { unzipText } from '../run-state.js';
import { Notice, SCREEN_TITLE } from './common.jsx';

export default function Pick({ error, busy, unzip, onPickFolder, onPickZip, onStop, onExportHelp, onDemo }) {
  const steps = [
    <>Запросите архив своих данных во ВКонтакте, отметив сообщения и фотографии. Как это сделать – на <Link onClick={onExportHelp}>странице помощи ВКонтакте</Link>.</>,
    'Когда придёт сообщение, что архив готов, скачайте его.',
    'Выберите здесь скачанный ZIP или папку, куда вы его распаковали.',
  ];
  return (
    <Group>
      <Box padding="m">
        <Text style={SCREEN_TITLE}>Сначала запросите архив во ВКонтакте</Text>
        <ol style={{ margin: 0, paddingLeft: 16 }}>
          {steps.map((t, i) => <li key={i} style={{ marginBottom: 8 }}><Text>{t}</Text></li>)}
        </ol>
      </Box>
      <Notice text={error} mode="error" />
      {unzip && (
        <Box padding="m">
          <div style={{ display: 'flex', gap: 12, alignItems: 'center' }} role="status" aria-live="polite">
            <Spinner size="s" />
            <Text>{unzipText(unzip)}</Text>
          </div>
        </Box>
      )}
      <Box padding="m">
        <ButtonGroup gap="m" style={{ flexWrap: 'wrap' }}>
          <Button size="l" onClick={onPickFolder} disabled={busy}>Выбрать папку</Button>
          <Button size="l" mode="secondary" onClick={onPickZip} disabled={busy}>Выбрать ZIP</Button>
          {unzip && <Button size="l" mode="secondary" onClick={onStop}>Остановить</Button>}
        </ButtonGroup>
        <Button size="s" mode="tertiary" style={{ paddingInlineStart: 0, marginTop: 8 }} onClick={onDemo} disabled={busy}>
          Нет своего архива? Открыть демо-данные
        </Button>
      </Box>
    </Group>
  );
}
