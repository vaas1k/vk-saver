import { Box, FormStatus, Spinner, Text } from '@vkontakte/vkui';

export function Busy() {
  return (
    <Box padding="m" role="status" aria-live="polite">
      <div style={{ display: 'flex', gap: 12, alignItems: 'center' }}>
        <Spinner size="s" style={{ width: 'auto', flex: 'none' }} />
        <Text>Подождите</Text>
      </div>
    </Box>
  );
}

export function Notice({ text, mode = 'default' }) {
  if (!text) return null;
  return <Box padding="m"><FormStatus mode={mode} role={mode === 'error' ? 'alert' : 'status'}>{text}</FormStatus></Box>;
}

export const LIST_ROW = { contentVisibility: 'auto', containIntrinsicSize: 'auto 40px' };
