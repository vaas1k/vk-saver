use serde_json::{Value, json};
use std::fs::{File, TryLockError};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use vk_engine::cdn::Cdn;
use vk_engine::clock::Clock;
use vk_engine::download::FreeSpace;
use vk_engine::error::Stop;
use vk_engine::export::index::Status;
use vk_engine::export::session::{Session, StartOptions};
use vk_engine::export::unzip;
use vk_engine::progress::{Event, Progress};

pub const ALREADY_OPEN: &str = "already_open";
pub const INTERNAL: &str = "internal";
pub const EXPORT_HELP: &str = "https://vk.ru/faq18145?lang=ru";
const LAST_EXPORT: &str = "last-export.txt";
const PROGRESS_GAP: Duration = Duration::from_millis(250);

pub trait Emit: Send + Sync {
    fn emit(&self, event: &str, payload: Value);
}

pub trait Opener: Send + Sync {
    fn open_path(&self, path: &Path) -> Result<(), String>;
    fn open_url(&self, url: &str) -> Result<(), String>;
}

pub struct Parts {
    pub cdn: Box<dyn Cdn>,
    pub space: Box<dyn FreeSpace>,
    pub clock: Box<dyn Clock>,
}

pub struct Deps {
    pub base: PathBuf,
    pub emit: Arc<dyn Emit>,
    pub opener: Arc<dyn Opener>,
    pub parts: Box<dyn Fn() -> Parts + Send>,
}

pub enum Job {
    Open(PathBuf),
    Unzip(PathBuf),
    Scan,
    Start(StartOptions),
    Result,
    Undo,
    SaveDiagnostics(PathBuf),
    OpenInBrowser,
    OpenFolder,
}

#[derive(Default)]
struct State {
    busy: bool,
    root: Option<String>,
    unzipping: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    idle: Condvar,
    cancel: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

type Reply = Sender<Value>;

/// One worker thread owns the session, so all SQLite stays on one thread.
#[derive(Clone)]
pub struct Backend {
    /// Err carries the error kind when startup failed.
    tx: Result<Sender<(Job, Reply)>, &'static str>,
    shared: Arc<Shared>,
    opener: Arc<dyn Opener>,
}

fn error(kind: &str) -> Value {
    json!({ "error": kind })
}

/// Err is the error kind: `already_open` only when another process holds the lock.
fn app_lock(base: &Path) -> Result<File, &'static str> {
    let storage = |_| Stop::Storage(String::new()).kind();
    std::fs::create_dir_all(base).map_err(storage)?;
    let f = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(base.join("app.lock"))
        .map_err(storage)?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(TryLockError::WouldBlock) => Err(ALREADY_OPEN),
        Err(TryLockError::Error(e)) => Err(storage(e)),
    }
}

impl Backend {
    pub fn start(deps: Deps) -> Backend {
        let shared = Arc::new(Shared::default());
        let opener = deps.opener.clone();
        let lock = match app_lock(&deps.base) {
            Ok(l) => l,
            Err(kind) => {
                return Backend {
                    tx: Err(kind),
                    shared,
                    opener,
                };
            }
        };
        let session = std::fs::read_to_string(deps.base.join(LAST_EXPORT))
            .ok()
            .and_then(|p| Session::open(Path::new(p.trim()), &deps.base).ok());
        shared.lock().root = session.as_ref().map(Session::root_display);
        let (tx, rx) = channel::<(Job, Reply)>();
        let sh = shared.clone();
        std::thread::spawn(move || {
            let _lock = lock;
            let mut w = Worker {
                deps,
                shared: sh,
                session,
            };
            for (job, reply) in rx {
                // A panic must not leave `busy` set: every later command would answer busy.
                let v = match catch_unwind(AssertUnwindSafe(|| w.job(job))) {
                    Ok(r) => r.unwrap_or_else(|s| error(s.kind())),
                    Err(_) => error(INTERNAL),
                };
                w.shared.lock().busy = false;
                w.shared.idle.notify_all();
                let _ = reply.send(v);
            }
        });
        Backend {
            tx: Ok(tx),
            shared,
            opener,
        }
    }

    /// Another process holds `app.lock`.
    pub fn already_open(&self) -> bool {
        matches!(self.tx, Err(ALREADY_OPEN))
    }

    /// Error kind when startup failed.
    pub fn start_error(&self) -> Option<&'static str> {
        self.tx.as_ref().err().copied()
    }

    pub fn last_export(&self) -> Option<String> {
        self.shared.lock().root.clone()
    }

    pub fn open_export_help(&self) -> Value {
        match self.opener.open_url(EXPORT_HELP) {
            Ok(()) => json!({}),
            Err(_) => error(INTERNAL),
        }
    }

    pub fn run(&self, job: Job) -> Result<Value, String> {
        let tx = match &self.tx {
            Ok(tx) => tx,
            Err(kind) => return Ok(error(kind)),
        };
        if let Job::OpenInBrowser | Job::OpenFolder = job {
            let Some(root) = self.last_export() else {
                return Ok(error(Stop::NotExport.kind()));
            };
            let mut path = PathBuf::from(root);
            if let Job::OpenInBrowser = job {
                path.push("index.html");
            }
            return Ok(match self.opener.open_path(&path) {
                Ok(()) => json!({}),
                Err(_) => error(Stop::Storage(String::new()).kind()),
            });
        }
        {
            let mut s = self.shared.lock();
            if s.busy {
                return Ok(error(Stop::Busy.kind()));
            }
            s.busy = true;
        }
        self.shared.cancel.store(false, Ordering::Relaxed);
        let (rtx, rrx) = channel();
        tx.send((job, rtx)).map_err(|e| e.to_string())?;
        rrx.recv().map_err(|e| e.to_string())
    }

    pub fn stop(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    pub fn unzipping(&self) -> bool {
        self.shared.lock().unzipping
    }

    pub fn busy(&self) -> bool {
        self.shared.lock().busy
    }

    /// False when the worker is still busy after `d`.
    pub fn wait_idle_for(&self, d: Duration) -> bool {
        let g = self.shared.lock();
        let (_g, res) = self
            .shared
            .idle
            .wait_timeout_while(g, d, |s| s.busy)
            .unwrap_or_else(|e| e.into_inner());
        !res.timed_out()
    }
}

struct Worker {
    deps: Deps,
    shared: Arc<Shared>,
    session: Option<Session>,
}

impl Worker {
    fn session(&mut self) -> Result<&mut Session, Stop> {
        self.session.as_mut().ok_or(Stop::NotExport)
    }

    fn phase(&self, name: &str) {
        self.deps.emit.emit("phase", json!({ "phase": name }));
    }

    fn job(&mut self, job: Job) -> Result<Value, Stop> {
        match job {
            Job::Unzip(zip) => {
                let parts = (self.deps.parts)();
                let shared = self.shared.clone();
                let meter = Meter::new(self.deps.emit.clone());
                self.phase("unzip");
                shared.lock().unzipping = true;
                let r = unzip::unzip(&zip, parts.space.as_ref(), &shared.cancel, &meter);
                shared.lock().unzipping = false;
                let root = r?;
                self.job(Job::Open(root))
            }
            Job::Open(dir) => {
                // The old session holds the export lock; a reopen of the same folder needs it released.
                self.session = None;
                self.shared.lock().root = None;
                let s = Session::open(&dir, &self.deps.base)?;
                let root = s.root_display();
                let _ = std::fs::write(self.deps.base.join(LAST_EXPORT), &root);
                self.shared.lock().root = Some(root.clone());
                self.session = Some(s);
                Ok(json!({ "root": root }))
            }
            Job::Scan => {
                let parts = (self.deps.parts)();
                let shared = self.shared.clone();
                self.session()?;
                self.phase("scan");
                let sum = self.session()?.scan(&shared.cancel, parts.space.as_ref())?;
                self.phase("ready");
                Ok(sum.to_json())
            }
            Job::Start(o) => {
                let parts = (self.deps.parts)();
                let shared = self.shared.clone();
                let meter = Meter::new(self.deps.emit.clone());
                self.session()?;
                self.phase("fetch");
                let r = self.session()?.start(
                    &o,
                    parts.cdn.as_ref(),
                    parts.space.as_ref(),
                    parts.clock.as_ref(),
                    &shared.cancel,
                    &meter,
                );
                self.phase("done");
                Ok(r.to_json())
            }
            Job::Result => Ok(self.session()?.result()?.to_json()),
            Job::Undo => {
                let r = self.session()?.undo()?;
                Ok(
                    json!({ "restored": r.restored, "busy": r.busy, "kept": r.kept, "skipped": r.skipped }),
                )
            }
            Job::SaveDiagnostics(path) => {
                let v = self.session()?.diagnostics(env!("CARGO_PKG_VERSION"))?;
                let bytes =
                    serde_json::to_vec_pretty(&v).map_err(|e| Stop::Storage(e.to_string()))?;
                let mut tmp = path.clone().into_os_string();
                tmp.push(".tmp");
                let tmp = PathBuf::from(tmp);
                let r = vk_engine::export::create_fresh(&tmp)
                    .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes))
                    .and_then(|()| std::fs::rename(&tmp, &path));
                if let Err(e) = r {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(Stop::Storage(e.to_string()));
                }
                Ok(json!({ "saved": true }))
            }
            Job::OpenInBrowser | Job::OpenFolder => unreachable!("handled in Backend::run"),
        }
    }
}

#[derive(Default)]
struct Tick {
    last: Option<Instant>,
    total: u64,
    done: u64,
    bytes: u64,
    saved: u64,
    gone: u64,
    failed: u64,
}

/// Turns engine events into `progress` (at most one per 250 ms) and `paused`: counts and section labels only.
pub struct Meter {
    emit: Arc<dyn Emit>,
    started: Instant,
    tick: Mutex<Tick>,
}

impl Meter {
    pub fn new(emit: Arc<dyn Emit>) -> Meter {
        Meter {
            emit,
            started: Instant::now(),
            tick: Mutex::new(Tick::default()),
        }
    }
}

impl Progress for Meter {
    fn event(&self, e: Event) {
        let mut t = self.tick.lock().unwrap_or_else(|e| e.into_inner());
        let section = match e {
            Event::Total(n) => {
                t.total = n;
                return;
            }
            Event::Unzip { done, total } => {
                let now = Instant::now();
                if done < total && t.last.is_some_and(|l| now - l < PROGRESS_GAP) {
                    return;
                }
                t.last = Some(now);
                self.emit
                    .emit("unzip", json!({ "done": done, "total": total }));
                return;
            }
            Event::Reused => return,
            Event::Paused { until_ms } => {
                self.emit.emit("paused", json!({ "until_ms": until_ms }));
                return;
            }
            Event::Bytes(n) => {
                t.bytes += n;
                None
            }
            Event::Item { section, status } => {
                t.done += 1;
                match status {
                    Status::Saved => t.saved += 1,
                    Status::Gone => t.gone += 1,
                    Status::Failed => t.failed += 1,
                    _ => {}
                }
                Some(section.as_str())
            }
        };
        let now = Instant::now();
        if t.last.is_some_and(|l| now - l < PROGRESS_GAP) {
            return;
        }
        t.last = Some(now);
        let secs = (now - self.started).as_secs_f64().max(0.001);
        let left = t.total.saturating_sub(t.done);
        let eta_s = if t.done == 0 {
            Value::Null
        } else {
            json!((secs * left as f64 / t.done as f64).ceil() as u64)
        };
        self.emit.emit(
            "progress",
            json!({
                "done": t.done,
                "total": t.total,
                "bytes": t.bytes,
                "mb_s": t.bytes as f64 / secs / 1e6,
                "eta_s": eta_s,
                "section": section,
                "saved": t.saved,
                "gone": t.gone,
                "failed": t.failed,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::io::Cursor;
    use vk_engine::cdn::{CdnResponse, NetError};
    use vk_engine::export::scan::Section;
    use vk_engine::testing::{FakeClock, FakeExportOpts, FakeSpace, fake_export, tmpdir};

    #[derive(Default)]
    struct Events(Mutex<Vec<(String, Value)>>);

    impl Emit for Events {
        fn emit(&self, event: &str, payload: Value) {
            self.0.lock().unwrap().push((event.into(), payload));
        }
    }

    #[derive(Default)]
    struct Opened(Mutex<Vec<PathBuf>>);

    impl Opener for Opened {
        fn open_path(&self, path: &Path) -> Result<(), String> {
            self.0.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }

        fn open_url(&self, url: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(PathBuf::from(url));
            Ok(())
        }
    }

    /// Serves a jpg for every url.
    struct AnyJpg;

    impl Cdn for AnyJpg {
        fn get(&self, _url: &str, _from: u64) -> Result<CdnResponse, NetError> {
            let mut b: Vec<u8> = (0..1000).map(|i| (i % 251) as u8).collect();
            b[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
            Ok(CdnResponse {
                status: 200,
                content_length: Some(1000),
                start: None,
                total: None,
                body: Box::new(Cursor::new(b)),
            })
        }
    }

    struct Env {
        backend: Backend,
        events: Arc<Events>,
        opened: Arc<Opened>,
    }

    fn env(base: &Path) -> Env {
        env_with(
            base,
            Box::new(|| Parts {
                cdn: Box::new(AnyJpg),
                space: Box::new(FakeSpace(Ok(10 << 30))),
                clock: Box::new(FakeClock::new().0),
            }),
        )
    }

    fn env_with(base: &Path, parts: Box<dyn Fn() -> Parts + Send>) -> Env {
        let events = Arc::new(Events::default());
        let opened = Arc::new(Opened::default());
        let backend = Backend::start(Deps {
            base: base.to_path_buf(),
            emit: events.clone(),
            opener: opened.clone(),
            parts,
        });
        Env {
            backend,
            events,
            opened,
        }
    }

    fn keys(v: &Value) -> BTreeSet<String> {
        v.as_object().unwrap().keys().cloned().collect()
    }

    fn set(k: &[&str]) -> BTreeSet<String> {
        k.iter().map(|s| s.to_string()).collect()
    }

    /// Open, scan, start, result; returns the root and the event names after the scan.
    fn flow(e: &Env, root: &Path) -> (String, Vec<(String, Value)>) {
        let b = &e.backend;
        let open = b.run(Job::Open(root.to_path_buf())).unwrap();
        assert_eq!(keys(&open), set(&["root"]));
        let sum = b.run(Job::Scan).unwrap();
        assert_eq!(
            keys(&sum),
            set(&[
                "root",
                "free_bytes",
                "albums",
                "messages",
                "voice",
                "other",
                "chats",
                "videos",
                "docs",
                "photo_links",
                "eta_s",
                "too_large"
            ])
        );
        let mark = e.events.0.lock().unwrap().len();
        let rep = b.run(Job::Start(StartOptions::default())).unwrap();
        assert_eq!(
            keys(&rep),
            set(&[
                "report_failed",
                "stop",
                "saved",
                "reused",
                "bytes",
                "gone",
                "host_dead",
                "failed",
                "rewrite"
            ])
        );
        assert_eq!(rep["stop"], Value::Null);
        assert!(rep["saved"].as_u64().unwrap() >= 3);
        let res = b.run(Job::Result).unwrap();
        assert_eq!(
            keys(&res),
            set(&[
                "saved",
                "bytes",
                "gone",
                "host_dead",
                "failed",
                "videos",
                "docs",
                "photo_links"
            ])
        );
        let ev = e.events.0.lock().unwrap()[mark..].to_vec();
        (open["root"].as_str().unwrap().to_string(), ev)
    }

    #[test]
    fn open_scan_start_result_contract() {
        let e = env(&tmpdir("backend-contract"));
        let root = fake_export("backend-contract-x", FakeExportOpts::default());
        let (_, ev) = flow(&e, &root);
        let names: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names.first(), Some(&"phase"));
        assert_eq!(names.last(), Some(&"phase"));
        assert!(names[1..names.len() - 1].iter().all(|n| *n == "progress"));
        assert!(names.len() >= 3, "at least one progress event");
        let all = serde_json::to_string(&ev).unwrap();
        assert!(!all.contains("http") && !all.contains("Тест") && !all.contains("vk-engine"));
        assert!(!e.backend.busy());
        let u = e.backend.run(Job::Undo).unwrap();
        assert_eq!(keys(&u), set(&["restored", "busy", "kept", "skipped"]));
        assert!(u["restored"].as_u64().unwrap() >= 1);
        assert_eq!(e.backend.run(Job::Undo).unwrap()["restored"], 0);
        let diag = tmpdir("backend-diag").join("diag.json");
        assert_eq!(
            e.backend.run(Job::SaveDiagnostics(diag.clone())).unwrap()["saved"],
            true
        );
        let text = std::fs::read_to_string(&diag).unwrap();
        assert!(text.contains("\"app\"") && !text.contains("://") && !text.contains("Тест"));
        assert!(!diag.with_file_name("diag.json.tmp").exists());
        let blocked = tmpdir("backend-diag-fail").join("diag.json");
        std::fs::create_dir_all(&blocked).unwrap();
        assert_eq!(
            e.backend
                .run(Job::SaveDiagnostics(blocked.clone()))
                .unwrap()["error"],
            "storage"
        );
        assert!(!blocked.with_file_name("diag.json.tmp").exists());
    }

    #[test]
    fn app_lock_second_instance() {
        let base = tmpdir("backend-lock");
        let first = env(&base);
        let second = env(&base);
        assert!(!first.backend.already_open());
        assert!(second.backend.already_open());
        assert_eq!(
            second.backend.run(Job::Result).unwrap(),
            json!({ "error": "already_open" })
        );
    }

    fn names(e: &Env) -> Vec<String> {
        e.events
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(n, p)| format!("{n}:{}", p["phase"].as_str().unwrap_or("")))
            .collect()
    }

    #[test]
    fn panic_in_job_keeps_worker_alive() {
        let first = AtomicBool::new(true);
        let e = env_with(
            &tmpdir("backend-panic"),
            Box::new(move || {
                assert!(!first.swap(false, Ordering::SeqCst), "boom");
                Parts {
                    cdn: Box::new(AnyJpg),
                    space: Box::new(FakeSpace(Ok(10 << 30))),
                    clock: Box::new(FakeClock::new().0),
                }
            }),
        );
        let root = fake_export("backend-panic-x", FakeExportOpts::default());
        e.backend.run(Job::Open(root)).unwrap();
        assert_eq!(
            e.backend.run(Job::Scan).unwrap(),
            json!({ "error": "internal" })
        );
        assert!(!e.backend.busy());
        assert!(e.backend.run(Job::Scan).unwrap().get("error").is_none());
    }

    #[test]
    fn app_lock_io_error_is_storage_not_already_open() {
        let base = tmpdir("backend-lock-io");
        let file = base.join("file");
        std::fs::write(&file, b"x").unwrap();
        let e = env(&file.join("sub"));
        assert!(!e.backend.already_open());
        assert_eq!(e.backend.start_error(), Some("storage"));
        assert_eq!(
            e.backend.run(Job::Result).unwrap(),
            json!({ "error": "storage" })
        );
    }

    #[test]
    fn no_session_emits_no_ready_and_no_fetch() {
        let e = env(&tmpdir("backend-nophase"));
        assert_eq!(
            e.backend.run(Job::Scan).unwrap(),
            json!({ "error": "not_export" })
        );
        assert_eq!(
            e.backend.run(Job::Start(StartOptions::default())).unwrap(),
            json!({ "error": "not_export" })
        );
        assert!(names(&e).is_empty(), "{:?}", names(&e));
    }

    #[test]
    fn meter_counts_by_status() {
        let events = Arc::new(Events::default());
        let m = Meter::new(events.clone());
        m.event(Event::Total(3));
        for st in [Status::Saved, Status::Gone, Status::Failed, Status::Saved] {
            m.event(Event::Item {
                section: Section::Albums,
                status: st,
            });
            std::thread::sleep(PROGRESS_GAP + Duration::from_millis(20));
        }
        let ev = events.0.lock().unwrap();
        let last = &ev.last().unwrap().1;
        assert_eq!(
            (
                last["saved"].as_u64(),
                last["gone"].as_u64(),
                last["failed"].as_u64()
            ),
            (Some(2), Some(1), Some(1))
        );
    }

    #[test]
    fn last_export_reopened() {
        let base = tmpdir("backend-last");
        let root = fake_export("backend-last-x", FakeExportOpts::default());
        let r = {
            let e = env(&base);
            let v = e.backend.run(Job::Open(root)).unwrap();
            assert!(e.backend.wait_idle_for(Duration::from_secs(5)));
            v["root"].as_str().unwrap().to_string()
        };
        // The first worker still holds app.lock and the export lock until its thread sees the channel close.
        let mut e = env(&base);
        for _ in 0..50 {
            if !e.backend.already_open() && e.backend.last_export().is_some() {
                break;
            }
            drop(e);
            std::thread::sleep(Duration::from_millis(20));
            e = env(&base);
        }
        assert_eq!(e.backend.last_export(), Some(r));
    }

    #[test]
    fn open_in_browser_opens_index_only() {
        let e = env(&tmpdir("backend-browser"));
        let root = fake_export("backend-browser-x", FakeExportOpts::default());
        let r = e.backend.run(Job::Open(root)).unwrap();
        assert_eq!(e.backend.run(Job::OpenInBrowser).unwrap(), json!({}));
        let want = Path::new(r["root"].as_str().unwrap()).join("index.html");
        assert_eq!(*e.opened.0.lock().unwrap(), vec![want]);
    }

    #[test]
    fn open_export_help_opens_fixed_url() {
        let e = env(&tmpdir("backend-help"));
        assert_eq!(e.backend.open_export_help(), json!({}));
        assert_eq!(
            *e.opened.0.lock().unwrap(),
            vec![PathBuf::from("https://vk.ru/faq18145?lang=ru")]
        );
    }

    #[test]
    fn errors_carry_no_paths() {
        let e = env(&tmpdir("backend-errors"));
        let not = tmpdir("backend-errors-not");
        assert_eq!(
            e.backend.run(Job::Open(not)).unwrap(),
            json!({ "error": "not_export" })
        );
        assert_eq!(
            e.backend.run(Job::Scan).unwrap(),
            json!({ "error": "not_export" })
        );
        assert_eq!(
            e.backend.run(Job::OpenFolder).unwrap(),
            json!({ "error": "not_export" })
        );
    }

    #[test]
    fn smoke_export_flow() {
        let e = env(&tmpdir("backend-smoke"));
        let root = fake_export("backend-smoke-x", FakeExportOpts::default());
        let (r, ev) = flow(&e, &root);
        assert!(!r.is_empty());
        let out = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("smoke-events.json");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        let json: Vec<Value> = ev
            .iter()
            .map(|(n, p)| json!({ "event": n, "payload": p }))
            .collect();
        std::fs::write(&out, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
        assert!(out.is_file());
    }
}
