import { Box, Button, ButtonGroup, Group, Header, Link, Text } from '@vkontakte/vkui';
import { unzipText } from '../run-state.js';
import { Notice } from './common.jsx';

export default function Pick({ error, busy, unzip, onPickFolder, onPickZip, onStop, onExportHelp }) {
  const steps = [
    <>Запросите архив своих данных во ВКонтакте, отметив сообщения и фотографии. Как это сделать – на <Link onClick={onExportHelp}>странице помощи ВКонтакте</Link>.</>,
    'Когда придёт сообщение, что архив готов, скачайте его.',
    'Выберите здесь скачанный ZIP или папку, куда вы его распаковали.',
  ];
  return (
    <Group header={<Header>Сначала запросите архив во ВКонтакте</Header>}>
      <Box padding="m">
        <ol style={{ margin: 0, paddingLeft: 20 }}>
          {steps.map((t, i) => <li key={i} style={{ marginBottom: 8 }}><Text>{t}</Text></li>)}
        </ol>
      </Box>
      <Notice text={error} mode="error" />
      {unzip && <Box padding="m"><Text>{unzipText(unzip)}</Text></Box>}
      <Box padding="m">
        <ButtonGroup gap="m" style={{ flexWrap: 'wrap' }}>
          <Button size="l" onClick={onPickFolder} disabled={busy}>Выбрать папку</Button>
          <Button size="l" mode="secondary" onClick={onPickZip} disabled={busy}>Выбрать ZIP</Button>
          {unzip && <Button size="l" mode="secondary" onClick={onStop}>Остановить</Button>}
        </ButtonGroup>
      </Box>
    </Group>
  );
}
