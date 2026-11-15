use super::index::{Row, open_hardened, recreate_db};
use super::{replace_file, safe_join, storage};
use crate::download::{FreeSpace, MAX_FILE, MIN_FREE, Media, Saved, sniff, with_suffix};
use crate::error::Stop;
use rusqlite::{Connection, ErrorCode, params};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const VERSION: i64 = 2;
const DDL: &str = "BEGIN;
CREATE TABLE known(match_key TEXT NOT NULL, root TEXT NOT NULL, local_path TEXT NOT NULL,
  sha256 TEXT NOT NULL, bytes INTEGER NOT NULL, PRIMARY KEY(match_key, root));
PRAGMA user_version = 2;
COMMIT;";
/// `remember` commits once per this many rows, and on `flush`.
const BATCH: u32 = 64;

/// Url path plus the `size` value: host and signature change between exports of one file, the path
/// carries VK's per-file random token. Ids (peer, message) repeat across accounts and are not used.
pub fn match_key(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split('#').next().unwrap_or_default();
    let (host_path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let path = host_path.find('/').map_or("/", |i| &host_path[i..]);
    let size = query
        .split('&')
        .find_map(|p| p.strip_prefix("size=").or((p == "size").then_some("")));
    match size {
        Some(size) => format!("{path}?size={size}"),
        None => path.to_owned(),
    }
}

/// Files saved by any export on this machine. Lives outside every export and is untrusted: a row
/// is only a hint, the placed file is verified. Holds private urls and paths: never logged.
pub struct Known {
    db: Connection,
    pending: Cell<u32>,
}

impl Known {
    /// `base` is the app's local data dir, passed in by the shell. Busy or unreadable known.db is an
    /// error (reuse off for the run), only a damaged one or another schema is recreated.
    pub fn open(base: &Path) -> Result<Known, Stop> {
        fs::create_dir_all(base).map_err(storage)?;
        let path = base.join("known.db");
        let current = open_hardened(&path).and_then(|c| {
            let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
            Ok((c, v))
        });
        let db = match current {
            Ok((c, VERSION)) => c,
            Ok((c, _)) => {
                drop(c);
                recreate_db(&path, DDL)?
            }
            Err(e)
                if matches!(
                    e.sqlite_error_code(),
                    Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt)
                ) =>
            {
                recreate_db(&path, DDL)?
            }
            Err(e) => return Err(storage(e)),
        };
        let known = Known {
            db,
            pending: Cell::new(0),
        };
        known.prune()?;
        Ok(known)
    }

    fn prune(&self) -> Result<(), Stop> {
        let roots: Vec<String> = self
            .db
            .prepare("SELECT DISTINCT root FROM known")
            .and_then(|mut st| st.query_map([], |r| r.get(0))?.collect())
            .map_err(storage)?;
        for root in roots.iter().filter(|r| !is_export(Path::new(r))) {
            self.db
                .execute("DELETE FROM known WHERE root = ?1", [root])
                .map_err(storage)?;
        }
        Ok(())
    }

    /// `root` is the canonical export root; `row` is saved, `local_path` with its extension.
    pub fn remember(&self, root: &Path, row: &Row) -> Result<(), Stop> {
        let Some(sha) = &row.sha256 else {
            return Ok(());
        };
        if self.db.is_autocommit() {
            self.db.execute_batch("BEGIN").map_err(storage)?;
        }
        self.db
            .execute(
                "INSERT OR REPLACE INTO known(match_key, root, local_path, sha256, bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![match_key(&row.url), root.to_string_lossy(), row.local_path, sha, row.bytes as i64],
            )
            .map_err(storage)?;
        self.pending.set(self.pending.get() + 1);
        if self.pending.get() >= BATCH {
            self.flush()?;
        }
        Ok(())
    }

    /// Commits the rows of `remember` not committed yet.
    pub fn flush(&self) -> Result<(), Stop> {
        self.pending.set(0);
        if self.db.is_autocommit() {
            return Ok(());
        }
        self.db.execute_batch("COMMIT").map_err(storage)
    }

    /// Places a file of another export at `<stem>.<ext>` and verifies the placed copy; rows whose
    /// file fails a check are dropped.
    pub fn reuse(
        &self,
        root: &Path,
        row: &Row,
        stem: &Path,
        cancel: &AtomicBool,
        space: &dyn FreeSpace,
    ) -> Result<Option<Saved>, Stop> {
        let key = match_key(&row.url);
        let found: Vec<(String, String, String, i64)> = self
            .db
            .prepare("SELECT root, local_path, sha256, bytes FROM known WHERE match_key = ?1 AND root != ?2")
            .and_then(|mut st| {
                st.query_map(params![key, root.to_string_lossy()], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect()
            })
            .map_err(storage)?;
        for (from, rel, sha256, bytes) in found {
            let checked = match source(Path::new(&from), &rel, bytes) {
                None => None,
                Some((src, media)) => {
                    let path = with_suffix(stem, media.ext());
                    if !place(&src, stem, &path, root, space) {
                        continue;
                    }
                    let ok = matches_file(&path, &sha256, bytes as u64, media, cancel);
                    if !matches!(ok, Ok(true)) {
                        let _ = fs::remove_file(&path);
                    }
                    ok?.then_some(Saved {
                        path,
                        bytes: bytes as u64,
                        media,
                        sha256,
                    })
                }
            };
            if checked.is_some() {
                return Ok(checked);
            }
            self.db
                .execute(
                    "DELETE FROM known WHERE match_key = ?1 AND root = ?2",
                    params![key, from],
                )
                .map_err(storage)?;
        }
        Ok(None)
    }
}

impl Drop for Known {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// A processed export reached without links or junctions: canonical root, real `vk-saver/index.db`.
fn is_export(root: &Path) -> bool {
    let plain = |p: &Path, dir: bool| {
        fs::symlink_metadata(p).is_ok_and(|m| if dir { m.is_dir() } else { m.is_file() })
    };
    root.is_absolute()
        && plain(root, true)
        && fs::canonicalize(root).is_ok_and(|c| c == root)
        && plain(&root.join("vk-saver"), true)
        && plain(&root.join("vk-saver").join("index.db"), false)
}

/// The source must be a plain file inside a real export, reached without links or junctions.
fn source(from: &Path, rel: &str, bytes: i64) -> Option<(PathBuf, Media)> {
    if !is_export(from) || !(0..=MAX_FILE as i64).contains(&bytes) {
        return None;
    }
    let src = safe_join(from, rel).ok()?;
    let mut p = from.to_path_buf();
    let mut meta = None;
    for c in rel.split('/') {
        p.push(c);
        let m = fs::symlink_metadata(&p).ok()?;
        if m.file_type().is_symlink() {
            return None;
        }
        meta = Some(m);
    }
    if !meta.is_some_and(|m| m.is_file() && m.len() == bytes as u64) {
        return None;
    }
    if !fs::canonicalize(&src)
        .ok()?
        .starts_with(fs::canonicalize(from).ok()?)
    {
        return None;
    }
    let mut head = Vec::with_capacity(16);
    File::open(&src)
        .and_then(|f| f.take(16).read_to_end(&mut head))
        .ok()?;
    Some((src, sniff(&head)?))
}

/// A hard link fails across volumes or onto an existing file; a copy covers both.
fn place(src: &Path, stem: &Path, dest: &Path, root: &Path, space: &dyn FreeSpace) -> bool {
    if dest
        .parent()
        .is_some_and(|d| fs::create_dir_all(d).is_err())
    {
        return false;
    }
    if fs::hard_link(src, dest).is_ok() {
        return true;
    }
    if !space.free_bytes(root).is_ok_and(|n| n >= MIN_FREE) {
        return false;
    }
    let tmp = with_suffix(stem, "reuse");
    let copied = fs::copy(src, &tmp).is_ok() && replace_file(&tmp, dest).is_ok();
    if !copied {
        let _ = fs::remove_file(&tmp);
    }
    copied
}

/// Size, sha256 and media type of the placed file; reads at most `MAX_FILE` + 1 bytes.
fn matches_file(
    path: &Path,
    sha256: &str,
    bytes: u64,
    media: Media,
    cancel: &AtomicBool,
) -> Result<bool, Stop> {
    let Ok(f) = File::open(path) else {
        return Ok(false);
    };
    let mut f = f.take(MAX_FILE + 1);
    let mut hasher = Sha256::new();
    let mut head = Vec::with_capacity(16);
    let mut read = 0u64;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return Ok(false),
        };
        let want = 16usize.saturating_sub(head.len()).min(n);
        head.extend_from_slice(&buf[..want]);
        hasher.update(&buf[..n]);
        read += n as u64;
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(read == bytes && hex == sha256 && sniff(&head) == Some(media))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::index::Status;
    use crate::export::session::{RunReport, Session, StartOptions};
    use crate::testing::{
        CdnLog, FakeCdn, FakeClock, FakeExportOpts, FakeFile, FakeSpace, NoProgress, URL_ALBUM_1,
        URL_OLD_HOST, fake_export, tmpdir,
    };
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    const PLENTY: u64 = 10 << 30;
    const ALL: [Status; 6] = [
        Status::New,
        Status::Saved,
        Status::Gone,
        Status::Failed,
        Status::HostDead,
        Status::NotMedia,
    ];
    const ALBUM_PAGE: &str = "photos/photo-albums/album-1.html";

    fn jpg(fill: u8) -> Vec<u8> {
        let mut b = vec![fill; 1000];
        b[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        b
    }

    fn session(name: &str, base: &Path) -> Session {
        let root = fake_export(name, FakeExportOpts::default());
        let mut s = Session::open(&root, base).unwrap();
        s.scan(&AtomicBool::new(false), &FakeSpace(Ok(PLENTY)))
            .unwrap();
        s
    }

    fn rows(s: &Session) -> Vec<Row> {
        s.index().rows(&ALL).unwrap()
    }

    fn row(s: &Session, url: &str) -> Row {
        rows(s).into_iter().find(|r| r.url == url).unwrap()
    }

    fn file(s: &Session, rel: &str) -> PathBuf {
        rel.split('/')
            .fold(s.root().to_path_buf(), |p, c| p.join(c))
    }

    /// Serves every indexed url with a jpg and runs one start.
    fn run(s: &mut Session) -> (RunReport, CdnLog) {
        let rows = rows(s);
        let (cdn, log) = FakeCdn::new(
            rows.iter()
                .map(|r| (r.url.as_str(), vec![FakeFile::ok(&jpg(7))]))
                .collect(),
        );
        let r = s.start(
            &StartOptions::default(),
            &cdn,
            &FakeSpace(Ok(PLENTY)),
            &FakeClock::new().0,
            &AtomicBool::new(false),
            &NoProgress,
        );
        assert_eq!(r.stop, None);
        (r, log)
    }

    fn got(log: &CdnLog) -> BTreeSet<String> {
        log.gets().into_iter().map(|g| g.0).collect()
    }

    /// (root, local_path) of every known.db entry for `key`.
    fn entries(base: &Path, key: &str) -> Vec<(String, String)> {
        let k = Known::open(base).unwrap();
        let mut st =
            k.db.prepare("SELECT root, local_path FROM known WHERE match_key = ?1")
                .unwrap();
        st.query_map([key], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn root_str(s: &Session) -> String {
        s.root().to_string_lossy().into_owned()
    }

    /// Path in A that known.db holds for URL_ALBUM_1's key.
    fn known_file(a: &Session, base: &Path) -> PathBuf {
        let (_, rel) = entries(base, &match_key(URL_ALBUM_1))
            .into_iter()
            .find(|(r, _)| *r == root_str(a))
            .expect("A remembered");
        file(a, &rel)
    }

    fn urls_with_key(s: &Session, key: &str) -> BTreeSet<String> {
        rows(s)
            .into_iter()
            .filter(|r| match_key(&r.url) == key)
            .map(|r| r.url)
            .collect()
    }

    #[test]
    fn second_export_reuses_without_requests() {
        let base = tmpdir("known-reuse-base");
        let mut a = session("known-reuse-a", &base);
        run(&mut a);
        let mut b = session("known-reuse-b", &base);
        let (r, log) = run(&mut b);
        assert!(log.gets().is_empty(), "B requested {:?}", got(&log));
        assert!(r.saved > 0);
        assert_eq!((r.reused, r.bytes), (r.saved, 0), "reused, not downloaded");
        assert_eq!(r.to_json()["reused"], r.saved);
        assert!(rows(&b).iter().all(|r| r.status == Status::Saved));
        for r in rows(&b) {
            assert!(
                file(&b, &r.local_path).is_file(),
                "{} missing",
                r.local_path
            );
        }
        let html =
            String::from_utf8_lossy(&std::fs::read(file(&b, ALBUM_PAGE)).unwrap()).into_owned();
        assert!(!html.contains(&URL_ALBUM_1.replace('&', "&amp;")));
        assert!(html.contains("media/"));
        let a_dir = a.root().to_path_buf();
        drop(a);
        std::fs::remove_dir_all(a_dir).unwrap();
        for r in rows(&b) {
            assert!(
                file(&b, &r.local_path).is_file(),
                "{} lost with A",
                r.local_path
            );
        }
    }

    #[test]
    fn hash_mismatch_downloads() {
        let base = tmpdir("known-hash-base");
        let mut a = session("known-hash-a", &base);
        run(&mut a);
        std::fs::write(known_file(&a, &base), jpg(9)).unwrap();
        let mut b = session("known-hash-b", &base);
        let (_, log) = run(&mut b);
        let key = match_key(URL_ALBUM_1);
        assert_eq!(got(&log), urls_with_key(&b, &key));
        let roots: Vec<String> = entries(&base, &key).into_iter().map(|e| e.0).collect();
        assert_eq!(roots, vec![root_str(&b)], "stale A row replaced by B");
    }

    #[test]
    fn moved_old_export_ignored() {
        let base = tmpdir("known-moved-base");
        let mut a = session("known-moved-a", &base);
        run(&mut a);
        let a_root = root_str(&a);
        let a_dir = a.root().to_path_buf();
        drop(a);
        std::fs::remove_dir_all(a_dir).unwrap();
        assert!(
            entries(&base, &match_key(URL_ALBUM_1)).is_empty(),
            "pruned at open"
        );
        let mut b = session("known-moved-b", &base);
        let (_, log) = run(&mut b);
        let all: BTreeSet<String> = rows(&b).into_iter().map(|r| r.url).collect();
        assert_eq!(got(&log), all);
        for r in rows(&b) {
            assert!(
                entries(&base, &match_key(&r.url))
                    .iter()
                    .all(|e| e.0 != a_root)
            );
        }
    }

    #[test]
    fn same_root_not_reused_from_itself() {
        let base = tmpdir("known-self-base");
        let mut a = session("known-self-a", &base);
        run(&mut a);
        let mut r = row(&a, URL_ALBUM_1);
        r.local_path = "photos/photo-albums/media/copy".into();
        let stem = file(&a, &r.local_path);
        let k = Known::open(&base).unwrap();
        let go = AtomicBool::new(false);
        let space = FakeSpace(Ok(PLENTY));
        assert_eq!(k.reuse(a.root(), &r, &stem, &go, &space).unwrap(), None);
        assert!(!stem.with_extension("jpg").exists());
        assert_eq!(
            entries(&base, &match_key(&r.url)).len(),
            1,
            "own entry kept"
        );
    }

    #[test]
    fn corrupt_known_db_recreated() {
        let base = tmpdir("known-corrupt-base");
        std::fs::write(base.join("known.db"), b"not a database at all").unwrap();
        let mut a = session("known-corrupt-a", &base);
        run(&mut a);
        assert_eq!(entries(&base, &match_key(URL_ALBUM_1)).len(), 1);
    }

    /// A saved, B scanned, both sharing `base`.
    fn pair(name: &str) -> (PathBuf, Session, Session) {
        let base = tmpdir(&format!("{name}-base"));
        let mut a = session(&format!("{name}-a"), &base);
        run(&mut a);
        let b = session(&format!("{name}-b"), &base);
        (base, a, b)
    }

    fn try_reuse(
        base: &Path,
        b: &Session,
        r: &Row,
        free: u64,
        cancel: bool,
    ) -> Result<Option<Saved>, Stop> {
        let k = Known::open(base).unwrap();
        let stem = file(b, &r.local_path);
        k.reuse(
            b.root(),
            r,
            &stem,
            &AtomicBool::new(cancel),
            &FakeSpace(Ok(free)),
        )
    }

    fn dest(b: &Session, r: &Row) -> PathBuf {
        with_suffix(&file(b, &r.local_path), "jpg")
    }

    /// Replaces every known.db row with one for URL_ALBUM_1.
    fn put(base: &Path, from: &Path, rel: &str, sha: &str, bytes: u64) {
        let k = Known::open(base).unwrap();
        k.db.execute("DELETE FROM known", []).unwrap();
        k.db.execute(
            "INSERT INTO known VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                match_key(URL_ALBUM_1),
                from.to_string_lossy(),
                rel,
                sha,
                bytes as i64
            ],
        )
        .unwrap();
    }

    #[test]
    fn match_key_is_path_and_size() {
        let k = match_key;
        assert_eq!(k(URL_ALBUM_1), "/impg/aA1bB2/5001.jpg?size=1280x960");
        let moved = "https://sun1-1.userapi.com/impg/aA1bB2/5001.jpg?sign=ff&size=1280x960&type=x";
        assert_eq!(k(moved), k(URL_ALBUM_1));
        assert_ne!(
            k(&URL_ALBUM_1.replace("1280x960", "640x480")),
            k(URL_ALBUM_1)
        );
        assert_ne!(k(&URL_ALBUM_1.replace("aA1bB2", "zZ9yY8")), k(URL_ALBUM_1));
        assert_eq!(k(URL_OLD_HOST), "/c1234/v1234/5a/x5007old.jpg");
        assert_ne!(k("https://h/p.jpg?size="), k("https://h/p.jpg"));
        assert_ne!(k("https://h/p.jpg?xsize=1"), k("https://h/p.jpg?size=1"));
    }

    #[test]
    fn reuse_needs_same_path_and_size() {
        let (base, _a, b) = pair("known-match");
        let r = row(&b, URL_ALBUM_1);
        let with = |url: String| Row { url, ..r.clone() };
        let other_size = with(URL_ALBUM_1.replace("1280x960", "640x480"));
        assert_eq!(
            try_reuse(&base, &b, &other_size, PLENTY, false).unwrap(),
            None
        );
        let other_path = with(URL_ALBUM_1.replace("aA1bB2", "zZ9yY8"));
        assert_eq!(other_path.url_key, r.url_key);
        assert_eq!(
            try_reuse(&base, &b, &other_path, PLENTY, false).unwrap(),
            None
        );
        assert!(!dest(&b, &r).exists());
        let other_host = with(
            URL_ALBUM_1
                .replace("sun9-1", "sun9-77")
                .replace("sign=0a1b5001", "sign=ffff"),
        );
        assert!(
            try_reuse(&base, &b, &other_host, PLENTY, false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn hostile_source_rejected() {
        let (base, a, b) = pair("known-hostile");
        let saved = row(&a, URL_ALBUM_1);
        let (sha, bytes) = (saved.sha256.clone().unwrap(), saved.bytes);
        let r = row(&b, URL_ALBUM_1);
        let plain = tmpdir("known-hostile-plain");
        let copy = file(&a, &saved.local_path)
            .strip_prefix(a.root())
            .map(|p| plain.join(p))
            .unwrap();
        fs::create_dir_all(copy.parent().unwrap()).unwrap();
        fs::copy(file(&a, &saved.local_path), &copy).unwrap();
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut cases = vec![
            (PathBuf::from("relative/export"), saved.local_path.clone()),
            (plain, saved.local_path.clone()),
            (
                a.root().to_path_buf(),
                format!("photos/../{}", saved.local_path),
            ),
        ];
        let noindex = fs::canonicalize(tmpdir("known-hostile-noindex")).unwrap();
        let c = noindex.join(file(&a, &saved.local_path).strip_prefix(a.root()).unwrap());
        fs::create_dir_all(c.parent().unwrap()).unwrap();
        fs::copy(file(&a, &saved.local_path), &c).unwrap();
        fs::create_dir(noindex.join("vk-saver")).unwrap();
        cases.push((noindex, saved.local_path.clone()));
        #[cfg(unix)]
        {
            let link_root = fs::canonicalize(tmpdir("known-hostile-rootlink"))
                .unwrap()
                .join("r");
            std::os::unix::fs::symlink(a.root(), &link_root).unwrap();
            cases.push((link_root, saved.local_path.clone()));
            let mirror = fs::canonicalize(tmpdir("known-hostile-mirror")).unwrap();
            let c = mirror.join(file(&a, &saved.local_path).strip_prefix(a.root()).unwrap());
            fs::create_dir_all(c.parent().unwrap()).unwrap();
            fs::copy(file(&a, &saved.local_path), &c).unwrap();
            std::os::unix::fs::symlink(a.root().join("vk-saver"), mirror.join("vk-saver")).unwrap();
            cases.push((mirror, saved.local_path.clone()));
            std::os::unix::fs::symlink(a.root().join("photos"), a.root().join("lnk")).unwrap();
            let rel = saved.local_path.replacen("photos/", "lnk/", 1);
            assert_ne!(rel, saved.local_path);
            cases.push((a.root().to_path_buf(), rel));
        }
        for (from, rel) in cases {
            put(&base, &from, &rel, &sha, bytes);
            assert_eq!(
                try_reuse(&base, &b, &r, PLENTY, false).unwrap(),
                None,
                "{from:?} {rel}"
            );
            assert!(!dest(&b, &r).exists());
            assert!(
                entries(&base, &match_key(URL_ALBUM_1)).is_empty(),
                "{rel} dropped"
            );
        }
    }

    #[test]
    fn bad_file_rejected() {
        let (base, a, b) = pair("known-badfile");
        let saved = row(&a, URL_ALBUM_1);
        let (sha, bytes, rel) = (
            saved.sha256.clone().unwrap(),
            saved.bytes,
            saved.local_path.clone(),
        );
        let src = file(&a, &rel);
        let r = row(&b, URL_ALBUM_1);
        let key = match_key(URL_ALBUM_1);
        let check = |what: &str| {
            assert_eq!(
                try_reuse(&base, &b, &r, PLENTY, false).unwrap(),
                None,
                "{what}"
            );
            assert!(!dest(&b, &r).exists(), "{what}: dest removed");
            assert!(entries(&base, &key).is_empty(), "{what}: row dropped");
        };
        put(&base, a.root(), &rel, &sha, bytes + 1);
        check("size");
        put(&base, a.root(), &rel, &"0".repeat(64), bytes);
        check("sha");
        put(&base, a.root(), &rel, &sha, bytes);
        fs::write(&src, jpg(9)).unwrap();
        check("swapped after the row was written");
        let text = vec![b'a'; bytes as usize];
        fs::write(&src, &text).unwrap();
        put(
            &base,
            a.root(),
            &rel,
            &crate::download::sha256_file(&src).unwrap(),
            bytes,
        );
        check("magic");
    }

    #[test]
    fn cancel_while_hashing() {
        let (base, _a, b) = pair("known-cancel");
        let r = row(&b, URL_ALBUM_1);
        assert_eq!(try_reuse(&base, &b, &r, PLENTY, true), Err(Stop::Cancelled));
        assert!(!dest(&b, &r).exists());
        assert_eq!(entries(&base, &match_key(URL_ALBUM_1)).len(), 1, "row kept");
    }

    /// An existing destination makes the hard link fail; the copy replaces it via its own temp name.
    #[test]
    fn copy_fallback_when_link_fails() {
        let (base, a, b) = pair("known-copy");
        let r = row(&b, URL_ALBUM_1);
        let stem = file(&b, &r.local_path);
        fs::create_dir_all(stem.parent().unwrap()).unwrap();
        fs::write(dest(&b, &r), b"old").unwrap();
        fs::write(with_suffix(&stem, "part"), b"resume").unwrap();
        assert_eq!(
            try_reuse(&base, &b, &r, 0, false).unwrap(),
            None,
            "copy needs MIN_FREE"
        );
        assert_eq!(fs::read(dest(&b, &r)).unwrap(), b"old");
        assert_eq!(entries(&base, &match_key(URL_ALBUM_1)).len(), 1, "row kept");
        assert!(try_reuse(&base, &b, &r, PLENTY, false).unwrap().is_some());
        assert_eq!(
            fs::read(dest(&b, &r)).unwrap(),
            fs::read(known_file(&a, &base)).unwrap()
        );
        assert_eq!(fs::read(with_suffix(&stem, "part")).unwrap(), b"resume");
        let left: Vec<_> = fs::read_dir(stem.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().ends_with(".reuse"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_known_db_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let base = tmpdir("known-unreadable-base");
        drop(Known::open(&base).unwrap());
        let db = base.join("known.db");
        let before = fs::read(&db).unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o000)).unwrap();
        let opened = Known::open(&base);
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(opened.is_err());
        assert_eq!(fs::read(&db).unwrap(), before);
    }

    /// Both exports see one file: deleting A keeps B (checked above), an in-place edit shows in both.
    #[test]
    fn hard_link_on_ntfs() {
        let base = tmpdir("known-link-base");
        let mut a = session("known-link-a", &base);
        run(&mut a);
        let mut b = session("known-link-b", &base);
        run(&mut b);
        let in_a = known_file(&a, &base);
        let in_b = file(&b, &row(&b, URL_ALBUM_1).local_path);
        let mut edited = jpg(7);
        edited[500] = 1;
        let mut f = std::fs::OpenOptions::new().write(true).open(&in_a).unwrap();
        std::io::Write::write_all(&mut f, &edited).unwrap();
        drop(f);
        assert_eq!(std::fs::read(in_b).unwrap(), edited);
    }

    #[test]
    fn known_db_opened_defensive() {
        use rusqlite::config::DbConfig;
        let k = Known::open(&tmpdir("known-defensive")).unwrap();
        assert!(
            !k.db
                .db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA)
                .unwrap()
        );
        assert!(k.db.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE).unwrap());
    }
}
