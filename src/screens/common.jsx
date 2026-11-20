import { Box, Button, FormStatus, PanelHeader, Spinner, Text } from '@vkontakte/vkui';

// Одна левая вертикальная линия для заголовков экранов и контента.
export const SCREEN_TITLE = { fontSize: 17, fontWeight: 600, marginBottom: 12 };

// Крупный заголовок приложения по центру плашки; клик возвращает на стартовый экран.
export function AppHeader({ onHome }) {
  return (
    <PanelHeader>
      <div style={{ display: 'flex', flex: 1, justifyContent: 'center' }}>
        <Button mode="tertiary" size="l" onClick={onHome} style={{ fontSize: 22, fontWeight: 600 }}>
          VK Saver
        </Button>
      </div>
    </PanelHeader>
  );
}

// Затемнение всего окна со спиннером на время ожидания (выбор архива, старт скачивания).
export function Overlay({ text = 'Подождите' }) {
  return (
    <div
      role="alert"
      aria-busy="true"
      aria-label={text}
      style={{
        position: 'fixed',
        inset: 0,
        zIndex: 1000,
        background: 'rgba(0, 0, 0, 0.4)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
      }}
    >
      <div
        style={{
          background: 'var(--vkui--color_background_content, #fff)',
          borderRadius: 12,
          padding: '24px 32px',
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'center',
          gap: 12,
        }}
      >
        <Spinner size="large" />
        <Text>{text}</Text>
      </div>
    </div>
  );
}

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
