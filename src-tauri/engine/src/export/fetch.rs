use super::index::{Index, Outcome, Row, Status};
use super::known::Known;
use super::safe_join;
use crate::cdn::{Cdn, NetError, Paced};
use crate::clock::Clock;
use crate::download::{DlError, FreeSpace, Saved, download};
use crate::error::Stop;
use crate::progress::{Event, Progress};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub const RETRY_MS: [u64; 2] = [5_000, 30_000];
pub const PAUSE_MS: u64 = 60_000;
pub const MAX_PAUSES: u32 = 3;
pub const DEAD_AFTER: u32 = 5;
pub const OFFLINE_AFTER: u32 = 10;
/// Longest sleep between cancel checks.
const WAIT_SLICE_MS: u64 = 1_000;

/// Host + first path segment: old `c<N>` storage shares hosts with live `impg` files.
pub fn host_key(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => {
            let seg = u
                .path()
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or("");
            format!("{}/{seg}", u.host_str().unwrap_or(""))
        }
        Err(_) => url.to_string(),
    }
}

pub struct Job {
    pub row: Row,
    pub date: Option<i64>,
}

pub struct FetchCtx<'a> {
    pub cdn: &'a dyn Cdn,
    pub space: &'a dyn FreeSpace,
    pub clock: &'a dyn Clock,
    pub cancel: &'a AtomicBool,
    pub progress: &'a dyn Progress,
    pub root: &'a Path,
    pub index: &'a Index,
    pub known: Option<&'a Known>,
}

/// One request at a time; every GET and every reuse goes through `Index::record`, so `attempts` counts tries.
pub fn fetch(ctx: &FetchCtx, jobs: &[Job]) -> Result<(), Stop> {
    ctx.progress.event(Event::Total(jobs.len() as u64));
    let mut run = Run {
        ctx,
        cdn: Paced {
            inner: ctx.cdn,
            clock: ctx.clock,
            last: Cell::new(None),
        },
        key_fails: HashMap::new(),
        pauses: 0,
        streak: 0,
        streak_keys: HashSet::new(),
        answered: false,
        since_answer: 0,
    };
    for job in jobs {
        let status = run.job(job)?;
        ctx.progress.event(Event::Item {
            section: job.row.section,
            status,
        });
    }
    // A key "died" while nothing answered since: the network may be down, not the key.
    if run.since_answer >= OFFLINE_AFTER {
        return Err(Stop::Network);
    }
    Ok(())
}

/// The file is saved either way; a lost mtime is cosmetic.
fn set_date(saved: &Saved, date: Option<i64>) {
    if let Some(date) = date {
        let _ = filetime::set_file_mtime(&saved.path, filetime::FileTime::from_unix_time(date, 0));
    }
}

struct Run<'r, 'a> {
    ctx: &'r FetchCtx<'a>,
    cdn: Paced<'a>,
    key_fails: HashMap<String, u32>,
    pauses: u32,
    /// Consecutive connect/timeout failures and the keys they hit; cleared when a key dies.
    streak: u32,
    streak_keys: HashSet<String>,
    /// Some request in this run got an HTTP answer.
    answered: bool,
    /// Connect/timeout failures since the last HTTP answer.
    since_answer: u32,
}

impl Run<'_, '_> {
    fn job(&mut self, job: &Job) -> Result<Status, Stop> {
        let ctx = self.ctx;
        let row = &job.row;
        if ctx.cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        let stem = safe_join(ctx.root, &row.local_path)?;
        // known.db only saves requests: its errors fall back to a download.
        match ctx
            .known
            .map(|k| k.reuse(ctx.root, row, &stem, ctx.cancel, ctx.space))
        {
            Some(Ok(Some(saved))) => {
                set_date(&saved, job.date);
                self.record(row, Status::Saved, None, Some(&saved))?;
                self.remember(row, &saved);
                ctx.progress.event(Event::Reused);
                return Ok(Status::Saved);
            }
            Some(Err(Stop::Cancelled)) => return Err(Stop::Cancelled),
            _ => {}
        }
        let key = host_key(&row.url);
        if self.key_fails.get(&key).is_some_and(|&n| n >= DEAD_AFTER) {
            self.record(row, Status::HostDead, None, None)?;
            return Ok(Status::HostDead);
        }
        let mut retries = 0;
        loop {
            let result = download(
                &self.cdn,
                ctx.space,
                &row.url,
                &stem,
                ctx.root,
                ctx.cancel,
                ctx.progress,
            );
            let offline = matches!(
                result,
                Err(DlError::Net(NetError::Connect | NetError::Timeout))
            );
            if offline {
                self.streak += 1;
                self.streak_keys.insert(key.clone());
                self.since_answer += 1;
            } else if matches!(
                result,
                Ok(_) | Err(DlError::Http(_) | DlError::NotMedia | DlError::TooLarge)
            ) {
                self.streak = 0;
                self.streak_keys.clear();
                self.since_answer = 0;
                self.answered = true;
            }
            // (status, http, retried, counts toward the key)
            let (status, http, retry, key_fail) = match &result {
                Ok(_) => (Status::Saved, Some(200), false, false),
                Err(DlError::Http(c @ (403 | 404 | 410))) => (Status::Gone, Some(*c), false, false),
                Err(DlError::Http(c @ (424 | 504))) => (Status::Failed, Some(*c), false, true),
                Err(DlError::Http(429)) => {
                    self.record(row, Status::Failed, Some(429), None)?;
                    if self.pauses == MAX_PAUSES {
                        return Err(Stop::Throttled);
                    }
                    let ms = PAUSE_MS << self.pauses;
                    self.pauses += 1;
                    ctx.progress.event(Event::Paused {
                        until_ms: ctx.clock.now_ms() + ms,
                    });
                    self.wait(ms)?;
                    continue;
                }
                Err(DlError::Http(c)) => (Status::Failed, Some(*c), *c >= 500, false),
                Err(DlError::NotMedia) => (Status::NotMedia, None, false, false),
                Err(DlError::TooLarge | DlError::Net(NetError::Refused)) => {
                    (Status::Failed, None, false, false)
                }
                // Without any answer in this run, no key can be told from our own network.
                Err(DlError::Net(NetError::Connect | NetError::Timeout)) => {
                    (Status::Failed, None, true, self.answered)
                }
                Err(DlError::Net(NetError::Other(_)) | DlError::Io(_)) => {
                    (Status::Failed, None, true, false)
                }
                Err(DlError::NoSpace(n)) => return Err(Stop::NoSpace(*n)),
                Err(DlError::Cancelled) => return Err(Stop::Cancelled),
            };
            if offline
                && self.streak >= OFFLINE_AFTER
                && (!self.answered || self.streak_keys.len() >= 2)
            {
                self.record(row, Status::Failed, None, None)?;
                return Err(Stop::Network);
            }
            if retry && retries < RETRY_MS.len() {
                self.record(row, Status::Failed, http, None)?;
                self.wait(RETRY_MS[retries])?;
                retries += 1;
                continue;
            }
            if let Ok(saved) = &result {
                set_date(saved, job.date);
            }
            self.record(row, status, http, result.as_ref().ok())?;
            if let Ok(saved) = &result {
                self.pauses = 0;
                self.remember(row, saved);
            }
            match status {
                Status::Failed if key_fail => {
                    let n = self.key_fails.entry(key).or_default();
                    *n += 1;
                    if *n >= DEAD_AFTER {
                        self.streak = 0;
                        self.streak_keys.clear();
                    }
                }
                Status::Saved | Status::Gone | Status::NotMedia => {
                    self.key_fails.remove(&key);
                }
                _ => {}
            }
            return Ok(status);
        }
    }

    fn record(
        &self,
        row: &Row,
        status: Status,
        http: Option<u16>,
        saved: Option<&Saved>,
    ) -> Result<(), Stop> {
        let o = Outcome {
            status,
            http,
            saved,
            now_s: (self.ctx.clock.now_ms() / 1000) as i64,
        };
        self.ctx.index.record(&row.url, &o)
    }

    /// `row` is the job's unsaved row; its `local_path` is the stem.
    fn remember(&self, row: &Row, saved: &Saved) {
        let Some(known) = self.ctx.known else { return };
        let row = Row {
            local_path: format!("{}.{}", row.local_path, saved.media.ext()),
            status: Status::Saved,
            bytes: saved.bytes,
            sha256: Some(saved.sha256.clone()),
            ..row.clone()
        };
        // A lost entry only costs a later download.
        let _ = known.remember(self.ctx.root, &row);
    }

    fn wait(&self, ms: u64) -> Result<(), Stop> {
        let clock = self.ctx.clock;
        let end = clock.now_ms() + ms;
        while clock.now_ms() < end {
            if self.ctx.cancel.load(Ordering::Relaxed) {
                return Err(Stop::Cancelled);
            }
            clock.sleep_ms((end - clock.now_ms()).min(WAIT_SLICE_MS));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdn::{CdnResponse, NetError};
    use crate::download::{MAX_FILE, MIN_FREE};
    use crate::export::scan::{Found, Key, Kind, Section};
    use crate::progress::Event;
    use crate::testing::{
        CdnLog, EventLog, FakeCdn, FakeClock, FakeFile, FakeSpace, NoProgress, tmpdir,
    };
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    const PAGE: &str = "messages/2000000101/messages0.html";

    fn jpg(len: usize) -> Vec<u8> {
        let mut b = vec![7u8; len];
        b[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        b
    }

    fn url(host: &str, seg: &str, n: usize) -> String {
        format!("https://{host}.userapi.com/{seg}/p{n}.jpg")
    }

    /// Records the fake clock at every GET start.
    struct Timed<'a> {
        inner: FakeCdn,
        clock: &'a FakeClock,
        starts: Mutex<Vec<u64>>,
    }

    impl Cdn for Timed<'_> {
        fn get(&self, url: &str, from: u64) -> Result<CdnResponse, NetError> {
            self.starts.lock().unwrap().push(self.clock.now_ms());
            self.inner.get(url, from)
        }
    }

    struct Env {
        root: PathBuf,
        index: Index,
        jobs: Vec<Job>,
        clock: FakeClock,
        cancel: AtomicBool,
    }

    fn env(name: &str, urls: &[String]) -> Env {
        let root = tmpdir(name);
        let state = root.join("vk-saver");
        std::fs::create_dir_all(&state).unwrap();
        let index = Index::open(&state).unwrap();
        let found: Vec<Found> = urls
            .iter()
            .map(|u| Found {
                url: u.clone(),
                page: PAGE.into(),
                section: Section::Messages,
                key: Key::Unknown,
                kind: Kind::Photo,
                date: None,
                size: None,
            })
            .collect();
        index.sync(&found).unwrap();
        let rows = index.rows(&[Status::New]).unwrap();
        let jobs = urls
            .iter()
            .map(|u| Job {
                row: rows.iter().find(|r| &r.url == u).unwrap().clone(),
                date: None,
            })
            .collect();
        Env {
            root,
            index,
            jobs,
            clock: FakeClock::new().0,
            cancel: AtomicBool::new(false),
        }
    }

    impl Env {
        fn run_with(&self, cdn: &dyn Cdn, progress: &dyn Progress) -> Result<(), Stop> {
            let space = FakeSpace(Ok(MIN_FREE));
            let ctx = FetchCtx {
                cdn,
                space: &space,
                clock: &self.clock,
                cancel: &self.cancel,
                progress,
                root: &self.root,
                index: &self.index,
                known: None,
            };
            fetch(&ctx, &self.jobs)
        }

        fn run(&self, cdn: &dyn Cdn) -> Result<(), Stop> {
            self.run_with(cdn, &NoProgress)
        }

        fn row(&self, url: &str) -> Row {
            let all = [
                Status::New,
                Status::Saved,
                Status::Gone,
                Status::Failed,
                Status::HostDead,
                Status::NotMedia,
            ];
            self.index
                .rows(&all)
                .unwrap()
                .into_iter()
                .find(|r| r.url == url)
                .unwrap()
        }

        fn status(&self, url: &str) -> Status {
            self.row(url).status
        }
    }

    fn gets_of(log: &CdnLog, url: &str) -> usize {
        log.gets().iter().filter(|(u, _)| u == url).count()
    }

    fn gaps(starts: &[u64]) -> Vec<u64> {
        starts.windows(2).map(|w| w[1] - w[0]).collect()
    }

    #[test]
    fn pace_two_per_second() {
        let urls: Vec<String> = (0..10).map(|n| url("sun9-1", "impg", n)).collect();
        let e = env("fetch-pace", &urls);
        let (inner, _) = FakeCdn::new(
            urls.iter()
                .map(|u| (u.as_str(), vec![FakeFile::ok(&jpg(100))]))
                .collect(),
        );
        let cdn = Timed {
            inner,
            clock: &e.clock,
            starts: Mutex::new(Vec::new()),
        };
        e.run(&cdn).unwrap();
        let starts = cdn.starts.lock().unwrap().clone();
        assert_eq!(starts.len(), 10);
        assert!(gaps(&starts).iter().all(|&g| g >= crate::export::GAP_MS));
        assert!(urls.iter().all(|u| e.status(u) == Status::Saved));
    }

    #[test]
    fn gone_codes_not_retried() {
        let urls: Vec<String> = (0..3).map(|n| url("sun9-1", "impg", n)).collect();
        let e = env("fetch-gone", &urls);
        let (cdn, log) = FakeCdn::new(vec![
            (&urls[0], vec![FakeFile::status(404)]),
            (&urls[1], vec![FakeFile::status(410)]),
            (&urls[2], vec![FakeFile::status(403)]),
        ]);
        e.run(&cdn).unwrap();
        assert_eq!(log.gets().len(), 3);
        for (u, code) in urls.iter().zip([404, 410, 403]) {
            let r = e.row(u);
            assert_eq!(
                (r.status, r.http, r.attempts),
                (Status::Gone, Some(code), 1)
            );
            assert!(r.tried_at.is_some());
        }
    }

    #[test]
    fn transient_retried_at_5_and_30() {
        let u = url("sun9-1", "impg", 1);
        let e = env("fetch-transient", std::slice::from_ref(&u));
        let (inner, _) = FakeCdn::new(vec![(
            &u,
            vec![
                FakeFile::status(500),
                FakeFile::status(500),
                FakeFile::ok(&jpg(100)),
            ],
        )]);
        let cdn = Timed {
            inner,
            clock: &e.clock,
            starts: Mutex::new(Vec::new()),
        };
        e.run(&cdn).unwrap();
        assert_eq!(gaps(&cdn.starts.lock().unwrap()), vec![5_000, 30_000]);
        let r = e.row(&u);
        assert_eq!(
            (r.status, r.attempts, r.http),
            (Status::Saved, 3, Some(200))
        );
    }

    #[test]
    fn dead_key_skips_rest_of_key() {
        let mut urls: Vec<String> = (0..6).map(|n| url("sun1-1", "c100", n)).collect();
        urls.push(url("sun1-1", "impg", 9));
        let e = env("fetch-dead", &urls);
        let mut files: Vec<(&str, Vec<FakeFile>)> = urls[..6]
            .iter()
            .map(|u| (u.as_str(), vec![FakeFile::status(504)]))
            .collect();
        files.push((&urls[6], vec![FakeFile::ok(&jpg(100))]));
        let (cdn, log) = FakeCdn::new(files);
        e.run(&cdn).unwrap();
        assert!(urls[..5].iter().all(|u| gets_of(&log, u) == 1));
        assert!(urls[..5].iter().all(|u| e.status(u) == Status::Failed));
        assert_eq!(gets_of(&log, &urls[5]), 0);
        assert_eq!(e.status(&urls[5]), Status::HostDead);
        assert_eq!(e.status(&urls[6]), Status::Saved);
    }

    #[test]
    fn success_resets_key_counter() {
        let urls: Vec<String> = (0..9).map(|n| url("sun1-1", "c100", n)).collect();
        let e = env("fetch-reset", &urls);
        let files = urls
            .iter()
            .enumerate()
            .map(|(i, u)| {
                let f = if i == 4 {
                    FakeFile::ok(&jpg(100))
                } else {
                    FakeFile::status(424)
                };
                (u.as_str(), vec![f])
            })
            .collect();
        let (cdn, log) = FakeCdn::new(files);
        e.run(&cdn).unwrap();
        assert_eq!(log.gets().len(), 9);
        assert!(urls.iter().all(|u| e.status(u) != Status::HostDead));
    }

    #[test]
    fn throttle_pauses_then_stops() {
        let urls: Vec<String> = (0..3).map(|n| url("sun9-1", "impg", n)).collect();
        let e = env("fetch-throttle", &urls);
        let (inner, _) = FakeCdn::new(
            urls.iter()
                .map(|u| (u.as_str(), vec![FakeFile::status(429)]))
                .collect(),
        );
        let cdn = Timed {
            inner,
            clock: &e.clock,
            starts: Mutex::new(Vec::new()),
        };
        let events = EventLog::default();
        assert_eq!(e.run_with(&cdn, &events), Err(Stop::Throttled));
        assert_eq!(
            gaps(&cdn.starts.lock().unwrap()),
            vec![PAUSE_MS, 2 * PAUSE_MS, 4 * PAUSE_MS]
        );
        let paused: Vec<u64> = events
            .events()
            .iter()
            .filter_map(|ev| match ev {
                Event::Paused { until_ms } => Some(*until_ms),
                _ => None,
            })
            .collect();
        assert_eq!(paused, vec![60_000, 180_000, 420_000]);
        assert!(urls[1..].iter().all(|u| e.status(u) == Status::New));
    }

    #[test]
    fn offline_mid_run_stops_as_network() {
        let urls: Vec<String> = (0..12)
            .map(|n| url(&format!("sun{n}-1"), "impg", n))
            .collect();
        let e = env("fetch-offline", &urls);
        let files = urls
            .iter()
            .enumerate()
            .map(|(i, u)| {
                let f = if i < 3 {
                    FakeFile::ok(&jpg(100))
                } else {
                    FakeFile::net(NetError::Connect)
                };
                (u.as_str(), vec![f])
            })
            .collect();
        let (cdn, log) = FakeCdn::new(files);
        assert_eq!(e.run(&cdn), Err(Stop::Network));
        assert_eq!(log.gets().len(), 3 + OFFLINE_AFTER as usize);
        assert!(urls[..3].iter().all(|u| e.status(u) == Status::Saved));
        // 3 rows x 3 attempts, then the first GET of the 4th failing row.
        assert!(urls[3..7].iter().all(|u| e.status(u) == Status::Failed));
        assert!(urls[7..].iter().all(|u| e.status(u) == Status::New));
    }

    #[test]
    fn two_dead_keys_then_live() {
        let mut urls = vec![url("sun9-1", "impg", 0)];
        urls.extend((0..6).map(|n| url("sun1-1", "c100", n)));
        urls.extend((0..6).map(|n| url("sun2-1", "c200", n)));
        urls.extend((1..3).map(|n| url("sun9-1", "impg", n)));
        let e = env("fetch-twodead", &urls);
        let files = urls
            .iter()
            .map(|u| {
                let f = if u.contains("/impg/") {
                    FakeFile::ok(&jpg(100))
                } else {
                    FakeFile::net(NetError::Connect)
                };
                (u.as_str(), vec![f])
            })
            .collect();
        let (cdn, log) = FakeCdn::new(files);
        assert_eq!(e.run(&cdn), Ok(()));
        assert_eq!(log.gets().len(), 3 + 2 * 15);
        assert_eq!(e.status(&urls[6]), Status::HostDead);
        assert_eq!(e.status(&urls[12]), Status::HostDead);
        assert!(urls[13..].iter().all(|u| e.status(u) == Status::Saved));
    }

    #[test]
    fn outage_from_start_on_one_key() {
        let urls: Vec<String> = (0..6).map(|n| url("sun1-1", "c100", n)).collect();
        let e = env("fetch-onekey", &urls);
        let (cdn, log) = FakeCdn::new(
            urls.iter()
                .map(|u| (u.as_str(), vec![FakeFile::net(NetError::Connect)]))
                .collect(),
        );
        assert_eq!(e.run(&cdn), Err(Stop::Network));
        assert_eq!(log.gets().len(), OFFLINE_AFTER as usize);
        assert!(urls.iter().all(|u| e.status(u) != Status::HostDead));
        assert!(urls[4..].iter().all(|u| e.status(u) == Status::New));
    }

    #[test]
    fn outage_after_saves_on_one_key() {
        // The key looks dead, but no answer came since the streak began: report Network.
        let urls: Vec<String> = (0..8).map(|n| url("sun9-1", "impg", n)).collect();
        let e = env("fetch-onekey-late", &urls);
        let files = urls
            .iter()
            .enumerate()
            .map(|(i, u)| {
                let f = if i < 2 {
                    FakeFile::ok(&jpg(100))
                } else {
                    FakeFile::net(NetError::Connect)
                };
                (u.as_str(), vec![f])
            })
            .collect();
        let (cdn, log) = FakeCdn::new(files);
        assert_eq!(e.run(&cdn), Err(Stop::Network));
        assert_eq!(log.gets().len(), 2 + 15);
        assert!(urls[2..7].iter().all(|u| e.status(u) == Status::Failed));
        assert_eq!(e.status(&urls[7]), Status::HostDead);
    }

    #[test]
    fn restart_inside_download_is_paced() {
        let u = url("sun9-1", "impg", 1);
        let e = env("fetch-restart", std::slice::from_ref(&u));
        let stem = crate::export::safe_join(&e.root, &e.jobs[0].row.local_path).unwrap();
        std::fs::create_dir_all(stem.parent().unwrap()).unwrap();
        std::fs::write(format!("{}.part", stem.display()), jpg(100)).unwrap();
        let (inner, log) = FakeCdn::new(vec![(&u, vec![FakeFile::ok(&jpg(100))])]);
        let cdn = Timed {
            inner,
            clock: &e.clock,
            starts: Mutex::new(Vec::new()),
        };
        e.run(&cdn).unwrap();
        assert_eq!(log.gets().len(), 2);
        assert!(gaps(&cdn.starts.lock().unwrap())[0] >= crate::export::GAP_MS);
        assert_eq!(e.status(&u), Status::Saved);
    }

    #[test]
    fn not_media_and_too_large_recorded() {
        let urls: Vec<String> = (0..2).map(|n| url("sun9-1", "impg", n)).collect();
        let e = env("fetch-notmedia", &urls);
        let big = FakeFile {
            content_length: Some(MAX_FILE + (1 << 20)),
            ..FakeFile::ok(&jpg(100))
        };
        let (cdn, log) = FakeCdn::new(vec![
            (
                &urls[0],
                vec![FakeFile::ok(b"<html><body>no</body></html>")],
            ),
            (&urls[1], vec![big]),
        ]);
        e.run(&cdn).unwrap();
        assert_eq!(log.gets().len(), 2);
        assert_eq!(e.status(&urls[0]), Status::NotMedia);
        assert_eq!(e.status(&urls[1]), Status::Failed);
        let media = e.root.join("messages").join("2000000101").join("media");
        let left = std::fs::read_dir(&media).map_or(0, |d| d.count());
        assert_eq!(left, 0);
    }

    #[test]
    fn mtime_from_date() {
        let u = url("sun9-1", "impg", 1);
        let mut e = env("fetch-mtime", std::slice::from_ref(&u));
        e.jobs[0].date = Some(1709631296);
        let (cdn, _) = FakeCdn::new(vec![(&u, vec![FakeFile::ok(&jpg(100))])]);
        e.run(&cdn).unwrap();
        let r = e.row(&u);
        let path = crate::export::safe_join(&e.root, &r.local_path).unwrap();
        let mtime = filetime::FileTime::from_last_modification_time(&path.metadata().unwrap());
        assert_eq!(mtime.unix_seconds(), 1709631296);
    }

    struct CancelOnBytes<'a>(&'a AtomicBool);

    impl Progress for CancelOnBytes<'_> {
        fn event(&self, e: Event) {
            if matches!(e, Event::Bytes(_)) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn cancel_keeps_part_and_row() {
        let urls: Vec<String> = (0..2).map(|n| url("sun9-1", "impg", n)).collect();
        let e = env("fetch-cancel", &urls);
        let (cdn, log) = FakeCdn::new(vec![
            (&urls[0], vec![FakeFile::ok(&jpg(3 << 20))]),
            (&urls[1], vec![FakeFile::ok(&jpg(100))]),
        ]);
        assert_eq!(
            e.run_with(&cdn, &CancelOnBytes(&e.cancel)),
            Err(Stop::Cancelled)
        );
        assert_eq!(log.gets().len(), 1);
        let r = e.row(&urls[0]);
        assert_eq!(r.status, Status::New);
        let stem = crate::export::safe_join(&e.root, &r.local_path).unwrap();
        let part = PathBuf::from(format!("{}.part", stem.display()));
        assert_eq!(part.metadata().unwrap().len(), 1 << 20);
    }

    #[test]
    fn events_total_items_bytes() {
        let urls: Vec<String> = (0..3).map(|n| url("sun9-1", "impg", n)).collect();
        let e = env("fetch-events", &urls);
        let (cdn, _) = FakeCdn::new(vec![
            (&urls[0], vec![FakeFile::ok(&jpg(100))]),
            (&urls[1], vec![FakeFile::status(404)]),
            (&urls[2], vec![FakeFile::ok(&jpg(250))]),
        ]);
        let events = EventLog::default();
        e.run_with(&cdn, &events).unwrap();
        let evs = events.events();
        let totals: Vec<&Event> = evs
            .iter()
            .filter(|v| matches!(v, Event::Total(_)))
            .collect();
        assert_eq!(totals, vec![&Event::Total(3)]);
        assert_eq!(evs[0], Event::Total(3));
        let items: Vec<Status> = evs
            .iter()
            .filter_map(|v| match v {
                Event::Item { section, status } => {
                    assert_eq!(*section, Section::Messages);
                    Some(*status)
                }
                _ => None,
            })
            .collect();
        assert_eq!(items, vec![Status::Saved, Status::Gone, Status::Saved]);
        let bytes: u64 = evs
            .iter()
            .filter_map(|v| match v {
                Event::Bytes(n) => Some(*n),
                _ => None,
            })
            .sum();
        let saved: u64 = urls.iter().map(|u| e.row(u).bytes).sum();
        assert_eq!((bytes, saved), (350, 350));
    }

    #[test]
    fn host_key_shapes() {
        assert_eq!(
            host_key("https://sun1-1.userapi.com/c1234/v1/a.jpg"),
            "sun1-1.userapi.com/c1234"
        );
        assert_eq!(
            host_key("https://sun1-1.userapi.com/impg/x?size=1"),
            "sun1-1.userapi.com/impg"
        );
        assert_eq!(
            host_key("https://sun1-1.userapi.com/"),
            "sun1-1.userapi.com/"
        );
    }
}
