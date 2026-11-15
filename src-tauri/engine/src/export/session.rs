use super::fetch::{FetchCtx, Job, fetch};
use super::index::{Index, Status};
use super::known::Known;
use super::report::{diagnostics, write_report};
use super::rewrite::{RewriteReport, UndoReport, rewrite_all, undo};
use super::scan::{ChatGroup, Scan, Summary, scan, summarize};
use super::{Export, display_path};
use crate::cdn::Cdn;
use crate::clock::Clock;
use crate::download::FreeSpace;
use crate::error::Stop;
use crate::progress::{Event, Progress};
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    pub excluded_peers: Vec<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunReport {
    pub stop: Option<Stop>,
    pub saved: u64,
    /// Of `saved`, taken from an older export; their bytes are not in `bytes`.
    pub reused: u64,
    pub bytes: u64,
    pub gone: u64,
    pub host_dead: u64,
    pub failed: u64,
    pub rewrite: RewriteReport,
    /// report.html could not be written (a convenience file: the run itself is unaffected).
    pub report_failed: bool,
}

impl RunReport {
    pub fn to_json(&self) -> Value {
        let r = &self.rewrite;
        json!({
            "stop": self.stop.as_ref().map(Stop::kind),
            "saved": self.saved,
            "reused": self.reused,
            "bytes": self.bytes,
            "gone": self.gone,
            "host_dead": self.host_dead,
            "failed": self.failed,
            "rewrite": { "pages": r.pages, "changed": r.changed, "busy": r.busy, "skipped": r.skipped },
            "report_failed": self.report_failed,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultView {
    pub saved: u64,
    pub bytes: u64,
    pub gone: u64,
    pub host_dead: u64,
    pub failed: u64,
    pub videos: u64,
    pub docs: u64,
    pub photo_links: u64,
    pub chats: Vec<ChatGroup>,
}

impl ResultView {
    pub fn to_json(&self) -> Value {
        json!({
            "saved": self.saved,
            "bytes": self.bytes,
            "gone": self.gone,
            "host_dead": self.host_dead,
            "failed": self.failed,
            "videos": self.videos,
            "docs": self.docs,
            "photo_links": self.photo_links,
        })
    }
}

pub struct Session {
    export: Export,
    index: Index,
    known: Option<Known>,
    scan: Option<Scan>,
}

/// Counts this run's items and bytes on the way to the caller's progress.
struct Tally<'a> {
    inner: &'a dyn Progress,
    saved: Cell<u64>,
    reused: Cell<u64>,
    bytes: Cell<u64>,
    gone: Cell<u64>,
    host_dead: Cell<u64>,
    failed: Cell<u64>,
}

impl Progress for Tally<'_> {
    fn event(&self, e: Event) {
        let bump = |c: &Cell<u64>, n: u64| c.set(c.get() + n);
        match &e {
            Event::Bytes(n) => bump(&self.bytes, *n),
            Event::Reused => bump(&self.reused, 1),
            Event::Item { status, .. } => match status {
                Status::Saved => bump(&self.saved, 1),
                Status::Gone => bump(&self.gone, 1),
                Status::HostDead => bump(&self.host_dead, 1),
                Status::Failed | Status::NotMedia => bump(&self.failed, 1),
                Status::New => {}
            },
            _ => {}
        }
        self.inner.event(e);
    }
}

fn storage(e: impl std::fmt::Display) -> Stop {
    Stop::Storage(e.to_string())
}

impl Session {
    /// `base` is the app's local data dir; known.db there only saves requests, so it never fails the open.
    pub fn open(dir: &Path, base: &Path) -> Result<Session, Stop> {
        let export = Export::open(dir)?;
        let index = Index::open(&export.state())?;
        Ok(Session {
            export,
            index,
            known: Known::open(base).ok(),
            scan: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn index(&self) -> &Index {
        &self.index
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        self.export.root()
    }

    pub fn root_display(&self) -> String {
        display_path(self.export.root())
    }

    pub fn scan(&mut self, cancel: &AtomicBool, space: &dyn FreeSpace) -> Result<Summary, Stop> {
        let root = self.export.root();
        let s = scan(root, cancel)?;
        self.index.sync(&s.found)?;
        self.index.reset_missing(root)?;
        let saved = self.index.saved_paths()?;
        let free = space.free_bytes(root).map_err(storage)?;
        let summary = summarize(&s, &|u| saved.contains_key(u), self.root_display(), free);
        self.scan = Some(s);
        Ok(summary)
    }

    pub fn start(
        &mut self,
        o: &StartOptions,
        cdn: &dyn Cdn,
        space: &dyn FreeSpace,
        clock: &dyn Clock,
        cancel: &AtomicBool,
        p: &dyn Progress,
    ) -> RunReport {
        if self.scan.is_none()
            && let Err(stop) = self.scan(cancel, space)
        {
            return RunReport {
                stop: Some(stop),
                ..RunReport::default()
            };
        }
        let tally = Tally {
            inner: p,
            saved: Cell::new(0),
            reused: Cell::new(0),
            bytes: Cell::new(0),
            gone: Cell::new(0),
            host_dead: Cell::new(0),
            failed: Cell::new(0),
        };
        let mut stop = self.jobs(o).and_then(|jobs| {
            let ctx = FetchCtx {
                cdn,
                space,
                clock,
                cancel,
                progress: &tally,
                root: self.export.root(),
                index: &self.index,
                known: self.known.as_ref(),
            };
            fetch(&ctx, &jobs)
        });
        if let Some(k) = &self.known {
            // Unflushed rows only cost later downloads.
            let _ = k.flush();
        }
        // Links of what is already saved are rewritten after any stop (spec:72, spec:95).
        let pages = &self.scan.as_ref().expect("scanned above").pages;
        let rewrite = match rewrite_all(self.export.root(), &self.index, pages) {
            Ok(r) => r,
            Err(e) => {
                stop = stop.and(Err(e));
                RewriteReport::default()
            }
        };
        // The report is a convenience: its failure must not mask the run's outcome.
        let report_failed = self.write_report().is_err();
        RunReport {
            stop: stop.err(),
            saved: tally.saved.get(),
            reused: tally.reused.get(),
            bytes: tally.bytes.get(),
            gone: tally.gone.get(),
            host_dead: tally.host_dead.get(),
            failed: tally.failed.get(),
            rewrite,
            report_failed,
        }
    }

    fn write_report(&self) -> Result<(), Stop> {
        write_report(&self.export.state(), &self.result()?)
    }

    /// Retryable rows minus URLs that the scan saw only in excluded chats.
    fn jobs(&self, o: &StartOptions) -> Result<Vec<Job>, Stop> {
        let scan = self.scan.as_ref().expect("scanned before jobs");
        let excluded: Vec<String> = o
            .excluded_peers
            .iter()
            .map(|p| format!("messages/{p}/"))
            .collect();
        // url -> (seen outside excluded chats, first known date)
        let mut seen: HashMap<&str, (bool, Option<i64>)> = HashMap::new();
        for f in &scan.found {
            let e = seen.entry(f.url.as_str()).or_default();
            e.0 |= !excluded.iter().any(|x| f.page.starts_with(x));
            e.1 = e.1.or(f.date);
        }
        let rows = self.index.rows(&[
            Status::New,
            Status::Failed,
            Status::HostDead,
            Status::NotMedia,
        ])?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let (keep, date) = seen.get(row.url.as_str()).copied().unwrap_or((true, None));
                keep.then_some(Job { row, date })
            })
            .collect())
    }

    pub fn undo(&self) -> Result<UndoReport, Stop> {
        let r = undo(self.export.root())?;
        // Best effort: the counts must not go stale, but the undo already happened.
        let _ = self.write_report();
        Ok(r)
    }

    pub fn result(&self) -> Result<ResultView, Stop> {
        let mut v = ResultView::default();
        for ((_, status), n) in self.index.counts()? {
            match status {
                Status::Saved => v.saved += n,
                Status::Gone => v.gone += n,
                Status::HostDead => v.host_dead += n,
                Status::Failed | Status::NotMedia => v.failed += n,
                Status::New => {}
            }
        }
        v.bytes = self
            .index
            .rows(&[Status::Saved])?
            .iter()
            .map(|r| r.bytes)
            .sum();
        if let Some(s) = &self.scan {
            for c in &s.chats {
                v.videos += c.videos;
                v.docs += c.docs;
                v.photo_links += c.photo_links;
            }
            let saved = self.index.saved_paths()?;
            v.chats = summarize(s, &|u| saved.contains_key(u), String::new(), 0).chats;
        }
        Ok(v)
    }

    pub fn diagnostics(&self, version: &str) -> Result<Value, Stop> {
        let empty = Scan::default();
        diagnostics(&self.index, self.scan.as_ref().unwrap_or(&empty), version)
    }
}

#[cfg(test)]
mod tests {
    use super::super::scan::Kind;
    use super::*;
    use crate::testing::{
        CdnLog, FakeCdn, FakeClock, FakeExportOpts, FakeFile, FakeSpace, NoProgress, PEER_A,
        PEER_B, URL_ALBUM_1, URL_ALBUM_2, URL_ALBUM_3, URL_AVATAR, URL_CHAT_1, URL_CHAT_2,
        URL_SHARED, URL_VOICE, fake_export,
    };
    use std::sync::atomic::Ordering;

    const PLENTY: u64 = 10 << 30;
    const ALL: [Status; 6] = [
        Status::New,
        Status::Saved,
        Status::Gone,
        Status::Failed,
        Status::HostDead,
        Status::NotMedia,
    ];

    fn jpg() -> Vec<u8> {
        let mut b: Vec<u8> = (0..1000).map(|i| (i % 251) as u8).collect();
        b[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        b
    }

    fn opened(name: &str) -> Session {
        let root = fake_export(name, FakeExportOpts::default());
        let mut s = Session::open(&root, &root.with_extension("base")).unwrap();
        s.scan(&AtomicBool::new(false), &FakeSpace(Ok(PLENTY)))
            .unwrap();
        s
    }

    /// Every indexed url answers: `jpg_for` urls with a jpg, the rest 404.
    fn serve(s: &Session, jpg_for: &dyn Fn(&str) -> bool) -> (FakeCdn, CdnLog) {
        serve_with(s, &|u| {
            if jpg_for(u) {
                FakeFile::ok(&jpg())
            } else {
                FakeFile::status(404)
            }
        })
    }

    fn serve_with(s: &Session, reply: &dyn Fn(&str) -> FakeFile) -> (FakeCdn, CdnLog) {
        let rows = s.index.rows(&ALL).unwrap();
        let files: Vec<(String, FakeFile)> = rows
            .iter()
            .map(|r| (r.url.clone(), reply(&r.url)))
            .collect();
        FakeCdn::new(
            files
                .iter()
                .map(|(u, f)| (u.as_str(), vec![f.clone()]))
                .collect(),
        )
    }

    fn run(s: &mut Session, o: &StartOptions, cdn: &FakeCdn, p: &dyn Progress) -> RunReport {
        s.start(
            o,
            cdn,
            &FakeSpace(Ok(PLENTY)),
            &FakeClock::new().0,
            &AtomicBool::new(false),
            p,
        )
    }

    fn status(s: &Session, url: &str) -> Status {
        let rows = s.index.rows(&ALL).unwrap();
        rows.into_iter().find(|r| r.url == url).unwrap().status
    }

    fn page(s: &Session, rel: &str) -> String {
        let p = rel
            .split('/')
            .fold(s.export.root().to_path_buf(), |p, c| p.join(c));
        String::from_utf8_lossy(&std::fs::read(p).unwrap()).into_owned()
    }

    fn esc(url: &str) -> String {
        url.replace('&', "&amp;")
    }

    const ALBUMS: [&str; 3] = [URL_ALBUM_1, URL_ALBUM_2, URL_ALBUM_3];
    const ALBUM_PAGE: &str = "photos/photo-albums/album-1.html";

    #[test]
    fn start_fetches_then_rewrites() {
        let mut s = opened("session-fetch");
        let (cdn, _) = serve(&s, &|u| ALBUMS.contains(&u));
        let r = run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        assert_eq!(r.stop, None);
        assert_eq!(r.saved, 3);
        assert_eq!(r.bytes, 3000);
        assert!(r.rewrite.changed >= 1);
        let html = page(&s, ALBUM_PAGE);
        for u in ALBUMS {
            assert_eq!(status(&s, u), Status::Saved);
            assert!(!html.contains(&esc(u)), "{u} still linked");
        }
        assert!(html.contains("media/"), "album page links local media");
        let v = s.result().unwrap();
        assert_eq!((v.saved, v.bytes), (3, 3000));
        assert_eq!(r.to_json()["stop"], Value::Null);
    }

    #[test]
    fn second_start_sends_nothing_for_saved() {
        let mut s = opened("session-second");
        let (cdn, log) = serve(&s, &|u| ALBUMS.contains(&u));
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        let before = page(&s, ALBUM_PAGE);
        let first = log.gets().len();
        let r = run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        assert_eq!(r.saved, 0);
        let again: Vec<String> = log.gets()[first..].iter().map(|g| g.0.clone()).collect();
        for u in ALBUMS {
            assert!(!again.iter().any(|g| g == u), "saved {u} requested again");
        }
        assert_eq!(page(&s, ALBUM_PAGE), before);
    }

    fn rescan(s: &mut Session) -> Summary {
        s.scan(&AtomicBool::new(false), &FakeSpace(Ok(PLENTY)))
            .unwrap()
    }

    fn backup_path(s: &Session, suffix: &str) -> std::path::PathBuf {
        let rel = format!("vk-saver/originals/{ALBUM_PAGE}{suffix}");
        let p = rel
            .split('/')
            .fold(s.export.root().to_path_buf(), |p, c| p.join(c));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        p
    }

    #[test]
    fn unreadable_sidecar_scans_live_page() {
        let mut s = opened("session-sidecar-dir");
        let found = rescan(&mut s).albums.count;
        std::fs::write(backup_path(&s, ""), "<html></html>").unwrap();
        std::fs::create_dir(backup_path(&s, ".outputs")).unwrap();
        assert_eq!(rescan(&mut s).albums.count, found);
        assert_eq!(status(&s, URL_ALBUM_2), Status::New);
    }

    #[test]
    fn oversized_backup_scans_live_page() {
        let mut s = opened("session-backup-big");
        let found = rescan(&mut s);
        std::fs::File::create(backup_path(&s, ""))
            .unwrap()
            .set_len(crate::export::MAX_PAGE + 1)
            .unwrap();
        let again = rescan(&mut s);
        assert_eq!(again.albums.count, found.albums.count);
        assert_eq!(again.too_large, found.too_large);
        assert_eq!(status(&s, URL_ALBUM_2), Status::New);
    }

    #[test]
    fn gone_photo_stays_unavailable_across_scans() {
        let mut s = opened("session-gone-stays");
        let (cdn, log) = serve(&s, &|u| u != URL_ALBUM_2);
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        assert_eq!(status(&s, URL_ALBUM_2), Status::Gone);
        let first = page(&s, ALBUM_PAGE);
        assert!(first.contains("vk-saver-missing"));
        let sent = log.gets().len();
        for _ in 0..2 {
            rescan(&mut s);
            run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
            assert_eq!(status(&s, URL_ALBUM_2), Status::Gone);
            assert_eq!(page(&s, ALBUM_PAGE), first);
        }
        assert!(!log.gets()[sent..].iter().any(|g| g.0 == URL_ALBUM_2));
    }

    #[test]
    fn failed_photo_saved_on_later_run() {
        let mut s = opened("session-failed-later");
        let found = rescan(&mut s).albums.count;
        let (cdn, _) = serve_with(&s, &|u| {
            if u == URL_ALBUM_2 {
                FakeFile::status(500)
            } else {
                FakeFile::ok(&jpg())
            }
        });
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        assert_eq!(status(&s, URL_ALBUM_2), Status::Failed);
        assert!(page(&s, ALBUM_PAGE).contains("vk-saver-missing"));
        assert_eq!(rescan(&mut s).albums.count, found);
        assert_eq!(status(&s, URL_ALBUM_2), Status::Failed);
        let (cdn, _) = serve(&s, &|_| true);
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        assert_eq!(status(&s, URL_ALBUM_2), Status::Saved);
        let html = page(&s, ALBUM_PAGE);
        assert!(!html.contains("vk-saver-missing"));
        assert!(!html.contains(&esc(URL_ALBUM_2)));
    }

    #[test]
    fn failed_chat_photo_saved_on_later_run() {
        let mut s = opened("session-chat-failed-later");
        let chat = format!("messages/{PEER_A}/messages0.html");
        rescan(&mut s);
        let (cdn, _) = serve_with(&s, &|u| {
            if u == URL_CHAT_1 {
                FakeFile::status(500)
            } else {
                FakeFile::ok(&jpg())
            }
        });
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        assert_eq!(status(&s, URL_CHAT_1), Status::Failed);
        let html = page(&s, &chat);
        assert!(html.contains("vk-saver-missing"));
        assert!(!html.contains(&esc(URL_CHAT_1)));
        rescan(&mut s);
        let (cdn, _) = serve(&s, &|_| true);
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        assert_eq!(status(&s, URL_CHAT_1), Status::Saved);
        let html = page(&s, &chat);
        assert!(!html.contains("vk-saver-missing"));
        assert!(!html.contains(&format!("href='{}'", esc(URL_CHAT_1))));
        let local = s.index.saved_paths().unwrap()[URL_CHAT_1].clone();
        let link = local.rsplit_once(&format!("messages/{PEER_A}/")).unwrap().1;
        assert!(html.contains(&format!("<a class='attachment__link' href='{link}'>")));
        assert!(html.contains(&format!("<img class=\"vk-saver-inline\" src=\"{link}\"")));
    }

    struct CancelAfter<'a>(&'a AtomicBool, Cell<u32>);

    impl Progress for CancelAfter<'_> {
        fn event(&self, e: Event) {
            if matches!(e, Event::Item { .. }) {
                self.1.set(self.1.get() + 1);
                if self.1.get() == 2 {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    #[test]
    fn stop_still_rewrites_done_part() {
        let mut s = opened("session-stop");
        let (cdn, _) = serve(&s, &|_| true);
        let cancel = AtomicBool::new(false);
        let p = CancelAfter(&cancel, Cell::new(0));
        let space = FakeSpace(Ok(PLENTY));
        let clock = FakeClock::new().0;
        let r = s.start(&StartOptions::default(), &cdn, &space, &clock, &cancel, &p);
        assert_eq!(r.stop, Some(Stop::Cancelled));
        assert_eq!(r.saved, 2);
        let rows = s.index.rows(&[Status::Saved]).unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            let html = page(&s, &row.page);
            let e = esc(&row.url);
            assert!(
                !html.contains(&format!("href='{e}'")) && !html.contains(&format!("src=\"{e}\"")),
                "{} not rewritten",
                row.url
            );
            assert!(html.contains("media/"), "{} links local media", row.page);
        }
    }

    #[test]
    fn excluded_chat_not_fetched_shared_url_is() {
        let mut s = opened("session-exclude-b");
        let (cdn, log) = serve(&s, &|_| true);
        let peer_b: i64 = PEER_B.parse().unwrap();
        run(
            &mut s,
            &StartOptions {
                excluded_peers: vec![peer_b],
            },
            &cdn,
            &NoProgress,
        );
        assert_eq!(status(&s, URL_SHARED), Status::Saved);
        assert!(log.gets().iter().any(|g| g.0 == URL_SHARED));

        let mut s = opened("session-exclude-a");
        let (cdn, log) = serve(&s, &|_| true);
        let peer_a: i64 = PEER_A.parse().unwrap();
        run(
            &mut s,
            &StartOptions {
                excluded_peers: vec![peer_a],
            },
            &cdn,
            &NoProgress,
        );
        assert_eq!(status(&s, URL_CHAT_1), Status::New);
        assert!(!log.gets().iter().any(|g| g.0 == URL_CHAT_1));
        assert_eq!(status(&s, URL_SHARED), Status::Saved);
    }

    #[test]
    fn deleted_media_refetched() {
        let mut s = opened("session-deleted");
        let (cdn, log) = serve(&s, &|u| ALBUMS.contains(&u));
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        let path = s.index.saved_paths().unwrap()[URL_ALBUM_1].clone();
        let file = super::super::safe_join(s.export.root(), &path).unwrap();
        std::fs::remove_file(&file).unwrap();
        s.scan(&AtomicBool::new(false), &FakeSpace(Ok(PLENTY)))
            .unwrap();
        assert_eq!(status(&s, URL_ALBUM_1), Status::New);
        let first = log.gets().len();
        let r = run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        assert_eq!(r.saved, 1);
        assert!(log.gets()[first..].iter().any(|g| g.0 == URL_ALBUM_1));
        assert!(file.is_file());
    }

    fn ogg() -> Vec<u8> {
        let mut b = jpg();
        b[..4].copy_from_slice(b"OggS");
        b
    }

    fn row(s: &Session, url: &str) -> super::super::index::Row {
        s.index
            .rows(&ALL)
            .unwrap()
            .into_iter()
            .find(|r| r.url == url)
            .unwrap()
    }

    #[test]
    fn voice_by_host_and_magic() {
        const PSV: &str = "https://psv4-2.userapi.com/s/v1/d/xY1/file_9001";
        let root = fake_export("session-voice", FakeExportOpts::default());
        let page = format!(
            "<div class=\"message\" data-id=\"9001\"><div class=\"attachment\"><div class=\"attachment__description\">Файл</div><a class='attachment__link' href='{PSV}'>f</a></div></div>"
        );
        std::fs::write(
            root.join("messages").join(PEER_A).join("messages100.html"),
            crate::export::text::encode_cp1251(&page),
        )
        .unwrap();
        let mut s = Session::open(&root, &root.with_extension("base")).unwrap();
        s.scan(&AtomicBool::new(false), &FakeSpace(Ok(PLENTY)))
            .unwrap();
        let kinds: Vec<Kind> = s
            .scan
            .as_ref()
            .unwrap()
            .found
            .iter()
            .filter(|f| f.url == PSV || f.url == URL_VOICE)
            .map(|f| f.kind)
            .collect();
        assert_eq!(kinds, [Kind::Voice, Kind::Voice]);
        let rows = s.index.rows(&ALL).unwrap();
        let cdn = FakeCdn::new(
            rows.iter()
                .map(|r| {
                    let f = match r.url.as_str() {
                        PSV => FakeFile::ok(&ogg()),
                        URL_VOICE => FakeFile::ok(&jpg()),
                        _ => FakeFile::status(404),
                    };
                    (r.url.as_str(), vec![f])
                })
                .collect(),
        )
        .0;
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        assert_eq!(
            row(&s, PSV).local_path,
            format!("messages/{PEER_A}/media/9001_1.ogg")
        );
        assert_eq!(
            row(&s, URL_VOICE).local_path,
            format!("messages/{PEER_A}/media/1002_3.jpg")
        );
    }

    #[test]
    fn shared_url_one_file_cross_link() {
        let mut s = opened("session-shared");
        let (cdn, log) = serve(&s, &|_| true);
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        let path = format!("messages/{PEER_A}/media/1002_2.jpg");
        assert_eq!(row(&s, URL_SHARED).local_path, path);
        assert_eq!(log.gets().iter().filter(|g| g.0 == URL_SHARED).count(), 1);
        let b = page(&s, &format!("messages/{PEER_B}/messages0.html"));
        assert!(
            b.contains(&format!("href='../{PEER_A}/media/1002_2.jpg'")),
            "{b}"
        );
        assert!(
            !s.export
                .root()
                .join("messages")
                .join(PEER_B)
                .join("media")
                .exists()
        );
    }

    #[test]
    fn avatar_named() {
        let mut s = opened("session-avatar");
        let (cdn, _) = serve(&s, &|u| u == URL_AVATAR);
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        assert_eq!(row(&s, URL_AVATAR).local_path, "profile/media/avatar.jpg");
        assert!(page(&s, "profile/page-info.html").contains("src=\"media/avatar.jpg\""));
    }

    #[test]
    fn slice_fallback_files_renamed() {
        let root = fake_export("session-slice", FakeExportOpts::default());
        let mut s = Session::open(&root, &root.with_extension("base")).unwrap();
        // the Task 9 slice keyed no message attachment
        let mut old = scan(&root, &AtomicBool::new(false)).unwrap();
        for f in &mut old.found {
            if f.section == super::super::scan::Section::Messages {
                f.key = super::super::scan::Key::Unknown;
            }
        }
        s.index.sync(&old.found).unwrap();
        s.scan = Some(old);
        let (cdn, _) = serve(&s, &|u| u == URL_CHAT_1);
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        let saved = row(&s, URL_CHAT_1).local_path;
        let u_dir = format!("messages/{PEER_A}/media/u_");
        assert!(saved.starts_with(&u_dir), "{saved}");
        assert!(row(&s, URL_CHAT_2).local_path.starts_with(&u_dir));

        s.scan(&AtomicBool::new(false), &FakeSpace(Ok(PLENTY)))
            .unwrap();
        assert_eq!(row(&s, URL_CHAT_1).local_path, saved);
        assert_eq!(row(&s, URL_CHAT_1).status, Status::Saved);
        assert_eq!(
            row(&s, URL_CHAT_2).local_path,
            format!("messages/{PEER_A}/media/1002_1")
        );
        let (cdn, _) = serve(&s, &|_| false);
        run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        let a0 = page(&s, &format!("messages/{PEER_A}/messages0.html"));
        let link = saved.rsplit_once(&format!("{PEER_A}/")).unwrap().1;
        assert!(a0.contains(&format!("href='{link}'")), "{a0}");
        assert!(
            super::super::safe_join(s.export.root(), &saved)
                .unwrap()
                .is_file()
        );
    }

    #[test]
    fn deep_cyrillic_root_end_to_end() {
        let deep = format!(
            "deep/Иван/{}/{}/{}",
            "д".repeat(60),
            "e".repeat(90),
            "ж".repeat(60)
        );
        let root = fake_export(&deep, FakeExportOpts::default());
        assert!(display_path(&root).chars().count() > 260);
        let mut s = Session::open(&root, &root.with_extension("base")).unwrap();
        let sum = s
            .scan(&AtomicBool::new(false), &FakeSpace(Ok(PLENTY)))
            .unwrap();
        assert!(!sum.root.starts_with(r"\\?\"));
        assert!(!s.root_display().starts_with(r"\\?\"));
        let (cdn, _) = serve(&s, &|u| ALBUMS.contains(&u));
        let r = run(&mut s, &StartOptions::default(), &cdn, &NoProgress);
        assert_eq!((r.stop, r.saved), (None, 3));
        assert!(!page(&s, ALBUM_PAGE).contains(&esc(URL_ALBUM_1)));
    }
}
