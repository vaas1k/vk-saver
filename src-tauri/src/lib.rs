pub mod backend;

use backend::{Backend, Deps, Emit, Job, Opener, Parts};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_opener::OpenerExt;
use vk_engine::cdn::HttpCdn;
use vk_engine::clock::SystemClock;
use vk_engine::download::DiskSpace;
use vk_engine::export::session::StartOptions;

const MAIN: &str = "main";
const CLOSE_WAIT: Duration = Duration::from_secs(30);

static CLOSE_ASKED: AtomicBool = AtomicBool::new(false);

struct TauriEmit(AppHandle);

impl Emit for TauriEmit {
    fn emit(&self, event: &str, payload: Value) {
        let _ = self.0.emit_to(MAIN, event, payload);
    }
}

/// The plugins are called from Rust only: the webview gets no opener or dialog permission.
struct PluginOpener(AppHandle);

impl Opener for PluginOpener {
    fn open_path(&self, path: &Path) -> Result<(), String> {
        self.0
            .opener()
            .open_path(path.to_string_lossy(), None::<&str>)
            .map_err(|e| e.to_string())
    }

    fn open_url(&self, url: &str) -> Result<(), String> {
        self.0
            .opener()
            .open_url(url, None::<&str>)
            .map_err(|e| e.to_string())
    }
}

fn internal() -> Value {
    json!({ "error": backend::INTERNAL })
}

async fn call(b: &Backend, job: Job) -> Value {
    let b = b.clone();
    match tauri::async_runtime::spawn_blocking(move || b.run(job)).await {
        Ok(Ok(v)) => v,
        _ => internal(),
    }
}

/// Rust opens the dialog: the frontend never sends a path.
#[tauri::command]
async fn pick_export(
    app: AppHandle,
    b: State<'_, Backend>,
    kind: String,
) -> Result<Option<Value>, String> {
    let zip = match kind.as_str() {
        "folder" => false,
        "zip" => true,
        _ => return Ok(None),
    };
    let Ok(picked) = tauri::async_runtime::spawn_blocking(move || {
        let d = app.dialog().file();
        if zip {
            d.add_filter("ZIP", &["zip"]).blocking_pick_file()
        } else {
            d.blocking_pick_folder()
        }
    })
    .await
    else {
        return Ok(Some(internal()));
    };
    let Some(path) = picked.and_then(|p| p.into_path().ok()) else {
        return Ok(None);
    };
    let job = if zip {
        Job::Unzip(path)
    } else {
        Job::Open(path)
    };
    Ok(Some(call(&b, job).await))
}

#[tauri::command]
fn last_export(b: State<'_, Backend>) -> Option<Value> {
    b.last_export().map(|root| json!({ "root": root }))
}

#[tauri::command]
async fn scan(b: State<'_, Backend>) -> Result<Value, String> {
    Ok(call(&b, Job::Scan).await)
}

#[tauri::command]
async fn start(b: State<'_, Backend>, excluded: Vec<String>) -> Result<Value, String> {
    let excluded_peers = excluded.iter().filter_map(|s| s.parse().ok()).collect();
    Ok(call(&b, Job::Start(StartOptions { excluded_peers })).await)
}

#[tauri::command]
fn stop(b: State<'_, Backend>) {
    b.stop();
}

#[tauri::command]
async fn result(b: State<'_, Backend>) -> Result<Value, String> {
    Ok(call(&b, Job::Result).await)
}

#[tauri::command]
async fn undo(b: State<'_, Backend>) -> Result<Value, String> {
    Ok(call(&b, Job::Undo).await)
}

/// Rust opens the save dialog: the frontend never sends a path.
#[tauri::command]
async fn save_diagnostics(app: AppHandle, b: State<'_, Backend>) -> Result<Option<Value>, String> {
    if b.busy() {
        return Ok(Some(json!({ "error": "busy" })));
    }
    let Ok(picked) = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_file_name("vk-saver-report.json")
            .add_filter("JSON", &["json"])
            .blocking_save_file()
    })
    .await
    else {
        return Ok(Some(internal()));
    };
    let Some(path) = picked.and_then(|p| p.into_path().ok()) else {
        return Ok(None);
    };
    Ok(Some(call(&b, Job::SaveDiagnostics(path)).await))
}

#[tauri::command]
async fn open_in_browser(b: State<'_, Backend>) -> Result<Value, String> {
    Ok(call(&b, Job::OpenInBrowser).await)
}

#[tauri::command]
async fn open_folder(b: State<'_, Backend>) -> Result<Value, String> {
    Ok(call(&b, Job::OpenFolder).await)
}

#[tauri::command]
fn open_export_help(b: State<'_, Backend>) -> Value {
    b.open_export_help()
}

pub fn run() {
    tauri::Builder::default()
        // registered first: plugin init scripts run in registration order
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("native-dialogs")
                .js_init_script(include_str!("native-dialogs.js").to_string())
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .setup(|app| {
            let base = app.path().local_data_dir()?.join("vk-saver");
            let h = app.handle().clone();
            let b = Backend::start(Deps {
                base,
                emit: Arc::new(TauriEmit(h.clone())),
                opener: Arc::new(PluginOpener(h.clone())),
                parts: Box::new(|| Parts {
                    cdn: Box::new(HttpCdn::new(env!("CARGO_PKG_VERSION"))),
                    space: Box::new(DiskSpace),
                    clock: Box::new(SystemClock),
                }),
            });
            if b.start_error().is_some() {
                let text = if b.already_open() {
                    "vk-saver уже открыт"
                } else {
                    "Не удалось открыть папку данных vk-saver"
                };
                std::thread::spawn(move || {
                    h.dialog().message(text).blocking_show();
                    h.exit(0);
                });
            }
            app.manage(b);
            Ok(())
        })
        .on_window_event(|w, e| {
            if let (WindowEvent::CloseRequested { api, .. }, Some(b)) =
                (e, w.try_state::<Backend>())
            {
                let b = b.inner().clone();
                if w.label() == MAIN && b.busy() {
                    api.prevent_close();
                    if CLOSE_ASKED.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    let app = w.app_handle().clone();
                    std::thread::spawn(move || {
                        let yes = app
                            .dialog()
                            .message(if b.unzipping() {
                                "Остановить распаковку и закрыть?"
                            } else {
                                "Остановить скачивание и закрыть окно?"
                            })
                            .buttons(MessageDialogButtons::YesNo)
                            .blocking_show();
                        if yes {
                            b.stop();
                            b.wait_idle_for(CLOSE_WAIT);
                            app.exit(0);
                        }
                        CLOSE_ASKED.store(false, Ordering::SeqCst);
                    });
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            pick_export,
            last_export,
            scan,
            start,
            stop,
            result,
            undo,
            save_diagnostics,
            open_in_browser,
            open_folder,
            open_export_help
        ])
        .run(tauri::generate_context!())
        .expect("error while running vk-saver");
}
