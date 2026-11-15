import { AdaptivityProvider, AppRoot, ConfigProvider, Panel, PanelHeader, SplitCol, SplitLayout, View } from '@vkontakte/vkui';
import { useEffect, useReducer, useRef, useState } from 'react';
import * as api from './api.js';
import { initial, reduce, stopText } from './run-state.js';
import Download from './screens/Download.jsx';
import Found from './screens/Found.jsx';
import Pick from './screens/Pick.jsx';
import Result from './screens/Result.jsx';
import { Notice } from './screens/common.jsx';

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
    setPicking(true);
    try {
      await opened(await api.pickExport(kind), kind === 'zip');
    } finally {
      setPicking(false);
    }
  };

  const undo = async () => {
    if (runLock.current) return;
    runLock.current = true;
    setRunning(true);
    try {
      dispatch({ type: 'undone', reply: await api.undo() });
    } catch {
      dispatch({ type: 'undone', reply: { error: 'internal' } });
    } finally {
      runLock.current = false;
      setRunning(false);
    }
  };

  const open = async (f) => dispatch({ type: 'notice', text: stopText((await f())?.error) });
  const saveReport = async () => {
    const r = await api.saveDiagnostics();
    if (!r) return;
    dispatch({ type: 'report_saved', error: r.error });
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
                  <PanelHeader>VK Saver</PanelHeader>
                  <Pick error={s.error} busy={picking || (s.screen === 'pick' && running)} unzip={s.unzip} onPickFolder={() => pick('folder')} onPickZip={() => pick('zip')} onStop={() => api.stop()} onExportHelp={() => api.openExportHelp()} />
                </Panel>
                <Panel id="found">
                  <PanelHeader>VK Saver</PanelHeader>
                  <Found summary={s.summary} excluded={s.excluded} chatKind={s.chatKind} onChatKind={(kind) => dispatch({ type: 'chat_kind', kind })} onToggle={(peer) => dispatch({ type: 'toggle', peer })} onToggleAll={() => dispatch({ type: 'toggle_all' })} onStart={run} onPickOther={() => dispatch({ type: 'pick' })} disabled={running} />
                </Panel>
                <Panel id="download">
                  <PanelHeader>VK Saver</PanelHeader>
                  <Download s={s} onStop={() => { dispatch({ type: 'stopping' }); api.stop(); }} />
                </Panel>
                <Panel id="result">
                  <PanelHeader>VK Saver</PanelHeader>
                  <Result s={s} onOpenBrowser={() => open(api.openInBrowser)} onOpenFolder={() => open(api.openFolder)} onResume={run} onPickOther={() => dispatch({ type: 'pick' })} onUndoAsk={() => dispatch({ type: 'undo_ask' })} onUndoCancel={() => dispatch({ type: 'undo_cancel' })} onUndo={undo} onSaveReport={() => saveReport()} disabled={running} />
                  <Notice text={s.notice} mode="error" />
                  <Notice text={s.reportText} />
                </Panel>
              </View>
            </SplitCol>
          </SplitLayout>
        </AppRoot>
      </AdaptivityProvider>
    </ConfigProvider>
  );
}
