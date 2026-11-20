import { AdaptivityProvider, AppRoot, ConfigProvider, Panel, SplitCol, SplitLayout, View } from '@vkontakte/vkui';
import { useEffect, useReducer, useRef, useState } from 'react';
import * as api from './api.js';
import { initial, reduce, stopText, unzipText } from './run-state.js';
import Download from './screens/Download.jsx';
import Found from './screens/Found.jsx';
import Pick from './screens/Pick.jsx';
import Result from './screens/Result.jsx';
import { AppHeader, Notice, Overlay } from './screens/common.jsx';

export default function App() {
  const [s, dispatch] = useReducer(reduce, initial);
  const [picking, setPicking] = useState(false);
  const [running, setRunning] = useState(false);
  const booted = useRef(false);
  const runLock = useRef(false);

  const opened = async (reply, zip = false) => {
    dispatch({ type: 'opened', reply, zip });
    if (reply?.root) dispatch({ type: 'scanned', reply: await api.scan() });
  };

  const run = async () => {
    if (runLock.current) return;
    runLock.current = true;
    setRunning(true);
    try {
      dispatch({ type: 'started' });
      const reply = await api.start(s.excluded);
      dispatch({ type: 'finished', reply });
      if (reply?.error === 'busy') return;
      dispatch({ type: 'result', reply: await api.result() });
    } finally {
      runLock.current = false;
      setRunning(false);
    }
  };

  const pick = async (kind) => {
    api.setDemo(false);
    setPicking(true);
    try {
      await opened(await api.pickExport(kind), kind === 'zip');
    } finally {
      setPicking(false);
    }
  };

  const demo = async () => {
    if (picking || running) return;
    api.setDemo(true);
    setPicking(true);
    try {
      await opened(await api.pickExport('folder'), false);
    } finally {
      setPicking(false);
    }
  };

  const open = async (f) => dispatch({ type: 'notice', text: stopText((await f())?.error) });

  const goHome = () => {
    // Возврат на стартовый экран. Если скачивание идёт, сначала останавливаем,
    // чтобы оно не продолжалось невидимым.
    if (s.screen === 'download' && s.progress) api.stop();
    dispatch({ type: 'pick' });
  };

  useEffect(() => {
    const offs = [
      api.on('progress', (p) => dispatch({ type: 'progress', p })),
      api.on('unzip', (p) => dispatch({ type: 'unzip', p })),
      api.on('paused', (p) => dispatch({ type: 'paused', until_ms: p.until_ms })),
    ];
    // StrictMode mounts twice in dev; a second scan would only get "busy".
    if (!booted.current) {
      booted.current = true;
      api.lastExport().then((r) => r?.root && opened(r));
    }
    return () => offs.forEach((p) => p.then((off) => off()));
  }, []);

  return (
    <ConfigProvider locale="ru">
      <AdaptivityProvider>
        <AppRoot>
          <SplitLayout>
            <SplitCol autoSpaced>
              <View activePanel={s.screen}>
                <Panel id="pick">
                  <AppHeader onHome={goHome} />
                  <Pick error={s.error} busy={picking || (s.screen === 'pick' && running)} unzip={s.unzip} onPickFolder={() => pick('folder')} onPickZip={() => pick('zip')} onStop={() => api.stop()} onExportHelp={() => api.openExportHelp()} onDemo={demo} />
                </Panel>
                <Panel id="found">
                  <AppHeader onHome={goHome} />
                  <Found summary={s.summary} excluded={s.excluded} chatKind={s.chatKind} onChatKind={(kind) => dispatch({ type: 'chat_kind', kind })} onToggle={(peer) => dispatch({ type: 'toggle', peer })} onToggleAll={() => dispatch({ type: 'toggle_all' })} onStart={run} onPickOther={() => dispatch({ type: 'pick' })} onOpenPage={(page) => open(() => api.openPage(page))} disabled={running} />
                </Panel>
                <Panel id="download">
                  <AppHeader onHome={goHome} />
                  <Download s={s} onStop={() => { dispatch({ type: 'stopping' }); api.stop(); }} />
                </Panel>
                <Panel id="result">
                  <AppHeader onHome={goHome} />
                  <Result s={s} onOpenBrowser={() => open(api.openInBrowser)} onOpenFolder={() => open(api.openFolder)} onResume={run} onPickOther={() => dispatch({ type: 'pick' })} disabled={running} />
                  <Notice text={s.notice} mode="error" />
                </Panel>
              </View>
              {(picking || running) && (
                <Overlay
                  text={
                    s.unzip ? unzipText(s.unzip)
                      : picking ? 'Сканируем архив'
                        : 'Подождите'
                  }
                />
              )}
            </SplitCol>
          </SplitLayout>
        </AppRoot>
      </AdaptivityProvider>
    </ConfigProvider>
  );
}
