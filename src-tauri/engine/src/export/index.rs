use super::scan::{Found, Key, Section};
use super::{safe_join, storage};
use crate::download::Saved;
use crate::error::Stop;
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

const VERSION: i64 = 2;
const DDL: &str = "BEGIN;
CREATE TABLE media(url TEXT PRIMARY KEY, url_key TEXT NOT NULL, section TEXT NOT NULL, page TEXT NOT NULL,
  local_path TEXT, status TEXT NOT NULL, http INTEGER, bytes INTEGER NOT NULL DEFAULT 0, sha256 TEXT,
  mime TEXT, tried_at INTEGER, attempts INTEGER NOT NULL DEFAULT 0);
CREATE UNIQUE INDEX media_path ON media(local_path COLLATE NOCASE);
PRAGMA user_version = 2;
COMMIT;";
const ROW: &str = "SELECT url, url_key, section, page, local_path, status, http, bytes, sha256, mime, tried_at, attempts FROM media ORDER BY attempts, rowid";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    New,
    Saved,
    Gone,
    Failed,
    HostDead,
    NotMedia,
}

const STATUSES: [Status; 6] = [
    Status::New,
    Status::Saved,
    Status::Gone,
    Status::Failed,
    Status::HostDead,
    Status::NotMedia,
];

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::New => "new",
            Status::Saved => "saved",
            Status::Gone => "gone",
            Status::Failed => "failed",
            Status::HostDead => "host_dead",
            Status::NotMedia => "not_media",
        }
    }

    fn parse(s: &str) -> Result<Status, Stop> {
        STATUSES
            .into_iter()
            .find(|v| v.as_str() == s)
            .ok_or_else(|| Stop::Storage(format!("bad status {s:?}")))
    }
}

fn parse_section(s: &str) -> Result<Section, Stop> {
    [
        Section::Albums,
        Section::Messages,
        Section::Profile,
        Section::Other,
    ]
    .into_iter()
    .find(|v| v.as_str() == s)
    .ok_or_else(|| Stop::Storage(format!("bad section {s:?}")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub url: String,
    pub url_key: String,
    pub section: Section,
    pub page: String,
    pub local_path: String,
    pub status: Status,
    pub http: Option<u16>,
    pub bytes: u64,
    pub sha256: Option<String>,
    pub mime: Option<String>,
    pub tried_at: Option<i64>,
    pub attempts: u32,
}

pub struct Outcome<'a> {
    pub status: Status,
    pub http: Option<u16>,
    pub saved: Option<&'a Saved>,
    pub now_s: i64,
}

fn url_path(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => u.path().to_string(),
        Err(_) => url.split(['?', '#']).next().unwrap_or("").to_string(),
    }
}

pub fn url_key(f: &Found) -> String {
    match f.key {
        Key::Album { owner, id } => format!("photo{owner}_{id}"),
        Key::Message { peer, msg, k } => format!("msg{peer}:{msg}:{k}"),
        Key::Avatar | Key::Unknown => url_path(&f.url),
    }
}

pub fn stem(f: &Found) -> String {
    match f.key {
        Key::Album { owner, id } => format!("photos/photo-albums/media/{owner}_{id}"),
        Key::Message { peer, msg, k } => format!("messages/{peer}/media/{msg}_{k}"),
        Key::Avatar => "profile/media/avatar".to_string(),
        Key::Unknown => {
            let hash = Sha256::digest(url_path(&f.url).as_bytes());
            let hex: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
            let dir = f.page.rsplit_once('/').map_or("", |(d, _)| d);
            let dir = if safe_join(Path::new(""), dir).is_ok() {
                dir
            } else {
                super::STATE_DIR
            };
            format!("{dir}/media/u_{hex}")
        }
    }
}

/// Stems never contain a dot, so a dot in the last component starts the extension.
fn stem_of(local_path: &str) -> &str {
    let name_at = local_path.rfind('/').map_or(0, |i| i + 1);
    match local_path[name_at..].rfind('.') {
        Some(i) => &local_path[..name_at + i],
        None => local_path,
    }
}

fn db(e: rusqlite::Error) -> Stop {
    storage(e)
}

/// A file that is not a database, or has another `user_version`, is deleted and recreated by `ddl`.
pub(super) fn open_db(path: &Path, version: i64, ddl: &str) -> Result<Connection, Stop> {
    let current = open_hardened(path).and_then(|c| {
        let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
        Ok((v == version).then_some(c))
    });
    if let Ok(Some(conn)) = current {
        return Ok(conn);
    }
    drop(current);
    recreate_db(path, ddl)
}

/// The file may come from someone else: its triggers and views get no untrusted functions.
pub(super) fn open_hardened(path: &Path) -> rusqlite::Result<Connection> {
    let c = Connection::open(path)?;
    c.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    c.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    Ok(c)
}

pub(super) fn recreate_db(path: &Path, ddl: &str) -> Result<Connection, Stop> {
    let mut journal = path.as_os_str().to_owned();
    journal.push("-journal");
    for p in [path, Path::new(&journal)] {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(storage(e)),
            _ => {}
        }
    }
    let conn = open_hardened(path).map_err(db)?;
    conn.execute_batch(ddl).map_err(db)?;
    Ok(conn)
}

pub struct Index {
    db: Connection,
}

impl Index {
    /// `state` is the export's validated `vk-saver` dir.
    pub fn open(state: &Path) -> Result<Index, Stop> {
        Ok(Index {
            db: open_db(&state.join("index.db"), VERSION, DDL)?,
        })
    }

    pub fn sync(&self, found: &[Found]) -> Result<(), Stop> {
        let mut seen = HashSet::new();
        let first: Vec<&Found> = found
            .iter()
            .filter(|f| seen.insert(f.url.as_str()))
            .collect();
        let tx = self.db.unchecked_transaction().map_err(db)?;
        let mut existing = HashMap::new();
        let mut taken = HashSet::new();
        let mut stale = Vec::new();
        {
            let mut st = tx
                .prepare("SELECT url, local_path, status FROM media")
                .map_err(db)?;
            let mut rows = st.query([]).map_err(db)?;
            while let Some(r) = rows.next().map_err(db)? {
                let url: String = r.get(0).map_err(db)?;
                let path: Option<String> = r.get(1).map_err(db)?;
                // An unsafe or missing path (hostile or damaged index.db): the row is rebuilt below.
                let Some(path) = path.filter(|p| safe_join(Path::new(""), p).is_ok()) else {
                    stale.push(url);
                    continue;
                };
                let status = Status::parse(&r.get::<_, String>(2).map_err(db)?)?;
                if status == Status::Saved {
                    taken.insert(stem_of(&path).to_ascii_lowercase());
                } else if !seen.contains(url.as_str()) {
                    stale.push(url);
                    continue;
                }
                existing.insert(url, status);
            }
        }
        for url in &stale {
            tx.execute("DELETE FROM media WHERE url = ?1", [url])
                .map_err(db)?;
        }
        // Frees the stems being reassigned so the NOCASE unique index sees no transient clash.
        for f in &first {
            tx.execute(
                "UPDATE media SET local_path = NULL WHERE url = ?1 AND status != 'saved'",
                [&f.url],
            )
            .map_err(db)?;
        }
        for f in first {
            let status = existing.get(&f.url);
            if status == Some(&Status::Saved) {
                continue;
            }
            let base = stem(f);
            let mut s = base.clone();
            let mut n = 1;
            while !taken.insert(s.to_ascii_lowercase()) {
                n += 1;
                s = format!("{base}_{n}");
            }
            let p = params![f.url, url_key(f), f.section.as_str(), f.page, s];
            let sql = if status.is_some() {
                "UPDATE media SET url_key = ?2, section = ?3, page = ?4, local_path = ?5 WHERE url = ?1"
            } else {
                "INSERT INTO media(url, url_key, section, page, local_path, status) VALUES (?1, ?2, ?3, ?4, ?5, 'new')"
            };
            tx.execute(sql, p).map_err(db)?;
        }
        tx.commit().map_err(db)
    }

    pub fn rows(&self, statuses: &[Status]) -> Result<Vec<Row>, Stop> {
        let mut st = self.db.prepare(ROW).map_err(db)?;
        let mut rows = st.query([]).map_err(db)?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(db)? {
            let status = Status::parse(&r.get::<_, String>(5).map_err(db)?)?;
            if !statuses.contains(&status) {
                continue;
            }
            out.push(Row {
                url: r.get(0).map_err(db)?,
                url_key: r.get(1).map_err(db)?,
                section: parse_section(&r.get::<_, String>(2).map_err(db)?)?,
                page: r.get(3).map_err(db)?,
                local_path: r.get(4).map_err(db)?,
                status,
                http: r.get(6).map_err(db)?,
                bytes: r.get::<_, i64>(7).map_err(db)? as u64,
                sha256: r.get(8).map_err(db)?,
                mime: r.get(9).map_err(db)?,
                tried_at: r.get(10).map_err(db)?,
                attempts: r.get(11).map_err(db)?,
            });
        }
        Ok(out)
    }

    pub fn record(&self, url: &str, o: &Outcome) -> Result<(), Stop> {
        let tx = self.db.unchecked_transaction().map_err(db)?;
        let path: String = tx
            .query_row("SELECT local_path FROM media WHERE url = ?1", [url], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db)?
            .ok_or_else(|| Stop::Storage("record of an unknown url".into()))?;
        tx.execute(
            "UPDATE media SET status = ?2, http = ?3, tried_at = ?4, attempts = attempts + 1 WHERE url = ?1",
            params![url, o.status.as_str(), o.http, o.now_s],
        )
        .map_err(db)?;
        if let Some(s) = o.saved {
            tx.execute(
                "UPDATE media SET local_path = ?2, bytes = ?3, sha256 = ?4, mime = ?5 WHERE url = ?1",
                params![
                    url,
                    format!("{}.{}", stem_of(&path), s.media.ext()),
                    s.bytes as i64,
                    s.sha256,
                    s.media.mime()
                ],
            )
            .map_err(db)?;
        }
        tx.commit().map_err(db)
    }

    pub fn saved_paths(&self) -> Result<HashMap<String, String>, Stop> {
        let mut st = self
            .db
            .prepare("SELECT url, local_path FROM media WHERE status = 'saved'")
            .map_err(db)?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)
    }

    /// URLs whose download ended `gone` or `failed`.
    pub fn unavailable_urls(&self) -> Result<HashSet<String>, Stop> {
        let mut st = self
            .db
            .prepare("SELECT url FROM media WHERE status IN ('gone', 'failed')")
            .map_err(db)?;
        st.query_map([], |r| r.get(0))
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)
    }

    /// `root` is the export root; returns how many rows went back to `new`.
    pub fn reset_missing(&self, root: &Path) -> Result<u64, Stop> {
        let tx = self.db.unchecked_transaction().map_err(db)?;
        let mut n = 0;
        for (url, path) in self.saved_paths()? {
            if safe_join(root, &path)?.is_file() {
                continue;
            }
            tx.execute(
                "UPDATE media SET status = 'new', local_path = ?2, bytes = 0, sha256 = NULL, mime = NULL WHERE url = ?1",
                params![url, stem_of(&path)],
            )
            .map_err(db)?;
            n += 1;
        }
        tx.commit().map_err(db)?;
        Ok(n)
    }

    pub fn counts(&self) -> Result<BTreeMap<(Section, Status), u64>, Stop> {
        let mut st = self
            .db
            .prepare("SELECT section, status, count(*) FROM media GROUP BY section, status")
            .map_err(db)?;
        let mut rows = st.query([]).map_err(db)?;
        let mut out = BTreeMap::new();
        while let Some(r) = rows.next().map_err(db)? {
            let section = parse_section(&r.get::<_, String>(0).map_err(db)?)?;
            let status = Status::parse(&r.get::<_, String>(1).map_err(db)?)?;
            out.insert((section, status), r.get::<_, i64>(2).map_err(db)? as u64);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::Media;
    use crate::export::scan::{Kind, scan};
    use crate::testing::{FakeExportOpts, fake_export, tmpdir};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    const U1: &str = "https://sun9-1.userapi.com/impg/aA1/5001.jpg?size=10x10&type=album";
    const U1B: &str = "https://sun9-2.userapi.com/impg/aA1/5001.jpg?size=20x20&type=album";
    const U2: &str = "https://sun9-3.userapi.com/impg/bB2/x.jpg?size=10x10";
    const U3: &str = "https://sun9-4.userapi.com/impg/bB2/x.jpg?size=20x20";

    fn found(url: &str, page: &str, key: Key) -> Found {
        Found {
            url: url.into(),
            page: page.into(),
            section: Section::of(page),
            key,
            kind: Kind::Photo,
            date: None,
            size: None,
        }
    }

    fn album() -> Key {
        Key::Album {
            owner: 200001,
            id: 5001,
        }
    }

    fn open(name: &str) -> (PathBuf, Index) {
        let root = tmpdir(name);
        let state = root.join("vk-saver");
        std::fs::create_dir_all(&state).unwrap();
        let ix = Index::open(&state).unwrap();
        (root, ix)
    }

    fn saved_png(path: PathBuf) -> Saved {
        Saved {
            path,
            bytes: 12,
            media: Media::Png,
            sha256: "ab".repeat(32),
        }
    }

    fn row(ix: &Index, url: &str) -> Row {
        let all = [
            Status::New,
            Status::Saved,
            Status::Gone,
            Status::Failed,
            Status::HostDead,
            Status::NotMedia,
        ];
        ix.rows(&all)
            .unwrap()
            .into_iter()
            .find(|r| r.url == url)
            .unwrap()
    }

    fn save(ix: &Index, url: &str, s: &Saved) {
        let o = Outcome {
            status: Status::Saved,
            http: Some(200),
            saved: Some(s),
            now_s: 1_700_000_000,
        };
        ix.record(url, &o).unwrap();
    }

    #[test]
    fn sync_inserts_distinct_urls_once() {
        let root = fake_export("index-sync", FakeExportOpts::default());
        let s = scan(&root, &AtomicBool::new(false)).unwrap();
        let state = root.join("vk-saver");
        std::fs::create_dir_all(&state).unwrap();
        let ix = Index::open(&state).unwrap();
        ix.sync(&s.found).unwrap();
        let distinct: HashSet<&str> = s.found.iter().map(|f| f.url.as_str()).collect();
        assert!(distinct.len() > 3);
        assert_eq!(ix.rows(&[Status::New]).unwrap().len(), distinct.len());
        ix.sync(&s.found).unwrap();
        assert_eq!(ix.rows(&[Status::New]).unwrap().len(), distinct.len());
        let c = ix.counts().unwrap();
        assert_eq!(c.values().sum::<u64>(), distinct.len() as u64);
    }

    #[test]
    fn stems_from_digits_only() {
        let a = found(U1, "photos/photo-albums/1.html", album());
        let m = found(
            U2,
            "messages/2000000101/messages0.html",
            Key::Message {
                peer: 2000000101,
                msg: 1001,
                k: 2,
            },
        );
        let av = found(U3, "profile/profile.html", Key::Avatar);
        let u = found(U2, "wall/wall0.html", Key::Unknown);
        let bad = found(U2, "wall/CON/w.html", Key::Unknown);
        let root_page = found(U2, "index.html", Key::Unknown);
        assert_eq!(stem(&a), "photos/photo-albums/media/200001_5001");
        assert_eq!(url_key(&a), "photo200001_5001");
        assert_eq!(stem(&m), "messages/2000000101/media/1001_2");
        assert_eq!(url_key(&m), "msg2000000101:1001:2");
        assert_eq!(stem(&av), "profile/media/avatar");
        assert_eq!(url_key(&av), "/impg/bB2/x.jpg");
        assert_eq!(url_key(&u), "/impg/bB2/x.jpg");
        let us = stem(&u);
        assert!(us.starts_with("wall/media/u_"), "{us}");
        assert_eq!(us.len(), "wall/media/u_".len() + 16);
        // Same URL path, other query: same fallback name.
        assert_eq!(
            us.rsplit('/').next(),
            stem(&found(U3, "wall/wall0.html", Key::Unknown))
                .rsplit('/')
                .next()
        );
        assert!(stem(&bad).starts_with("vk-saver/media/u_"));
        let neg = found(
            U2,
            "photos/photo-albums/2.html",
            Key::Album {
                owner: -200003,
                id: 5002,
            },
        );
        assert_eq!(stem(&neg), "photos/photo-albums/media/-200003_5002");
        for f in [&a, &m, &av, &u, &bad, &root_page, &neg] {
            safe_join(Path::new("r"), &stem(f)).unwrap();
        }
    }

    #[test]
    fn stem_collision_gets_suffix() {
        let (_r, ix) = open("index-collide");
        let p = "photos/photo-albums/1.html";
        ix.sync(&[found(U1, p, album()), found(U1B, p, album())])
            .unwrap();
        assert_eq!(
            row(&ix, U1).local_path,
            "photos/photo-albums/media/200001_5001"
        );
        assert_eq!(
            row(&ix, U1B).local_path,
            "photos/photo-albums/media/200001_5001_2"
        );
    }

    #[test]
    fn nocase_unique_path() {
        let (_r, ix) = open("index-nocase");
        ix.sync(&[
            found(U2, "A/p.html", Key::Unknown),
            found(U3, "a/p.html", Key::Unknown),
        ])
        .unwrap();
        let a = row(&ix, U2).local_path;
        let b = row(&ix, U3).local_path;
        assert!(a.starts_with("A/media/u_"));
        assert_eq!(b, format!("{}_2", a.replacen('A', "a", 1)));
    }

    #[test]
    fn record_saved_sets_ext() {
        let (r, ix) = open("index-record");
        ix.sync(&[found(U1, "photos/photo-albums/1.html", album())])
            .unwrap();
        let s = saved_png(r.join("x"));
        save(&ix, U1, &s);
        let got = row(&ix, U1);
        assert_eq!(got.local_path, "photos/photo-albums/media/200001_5001.png");
        assert_eq!(got.status, Status::Saved);
        assert_eq!(got.http, Some(200));
        assert_eq!(got.bytes, 12);
        assert_eq!(got.sha256.as_deref(), Some(s.sha256.as_str()));
        assert_eq!(got.mime.as_deref(), Some("image/png"));
        assert_eq!(got.tried_at, Some(1_700_000_000));
        assert_eq!(got.attempts, 1);
        assert_eq!(
            ix.saved_paths().unwrap().get(U1).map(String::as_str),
            Some("photos/photo-albums/media/200001_5001.png")
        );
        assert_eq!(Status::HostDead.as_str(), "host_dead");
        assert_eq!(Status::NotMedia.as_str(), "not_media");
    }

    #[test]
    fn unsaved_stem_recomputed_saved_kept() {
        let (r, ix) = open("index-recompute");
        let p = "messages/2000000101/messages0.html";
        ix.sync(&[found(U1, p, Key::Unknown), found(U2, p, Key::Unknown)])
            .unwrap();
        let s = saved_png(r.join("x"));
        save(&ix, U2, &s);
        let kept = row(&ix, U2);
        let msg = |k| Key::Message {
            peer: 2000000101,
            msg: 1001,
            k,
        };
        ix.sync(&[found(U1, p, msg(0)), found(U2, p, msg(1))])
            .unwrap();
        let renamed = row(&ix, U1);
        assert_eq!(renamed.local_path, "messages/2000000101/media/1001_0");
        assert_eq!(renamed.url_key, "msg2000000101:1001:0");
        assert_eq!(row(&ix, U2), kept);
        assert!(kept.local_path.contains("/media/u_"));
    }

    #[test]
    fn reset_missing_requeues() {
        let (r, ix) = open("index-reset");
        ix.sync(&[
            found(U1, "photos/photo-albums/1.html", album()),
            found(U2, "wall/wall0.html", Key::Unknown),
        ])
        .unwrap();
        let s = saved_png(r.join("x"));
        save(&ix, U1, &s);
        save(&ix, U2, &s);
        for p in ix.saved_paths().unwrap().values() {
            let f = safe_join(&r, p).unwrap();
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"x").unwrap();
        }
        let gone = safe_join(&r, &row(&ix, U1).local_path).unwrap();
        std::fs::remove_file(gone).unwrap();
        assert_eq!(ix.reset_missing(&r).unwrap(), 1);
        let back = row(&ix, U1);
        assert_eq!(back.status, Status::New);
        assert_eq!(back.local_path, "photos/photo-albums/media/200001_5001");
        assert_eq!(row(&ix, U2).status, Status::Saved);
        assert_eq!(ix.reset_missing(&r).unwrap(), 0);
    }

    #[test]
    fn rows_untried_first() {
        let (_r, ix) = open("index-order");
        let p = "messages/2000000101/messages0.html";
        ix.sync(&[
            found(U1, p, album()),
            found(U2, p, Key::Unknown),
            found(U3, p, Key::Unknown),
        ])
        .unwrap();
        let failed = Outcome {
            status: Status::Failed,
            http: Some(500),
            saved: None,
            now_s: 1_700_000_000,
        };
        ix.record(U1, &failed).unwrap();
        ix.record(U1, &failed).unwrap();
        ix.record(U3, &failed).unwrap();
        let urls: Vec<String> = ix
            .rows(&[Status::New, Status::Failed])
            .unwrap()
            .into_iter()
            .map(|r| r.url)
            .collect();
        assert_eq!(urls, [U2, U3, U1]);
    }

    #[test]
    fn unknown_version_recreated() {
        let (r, ix) = open("index-version");
        ix.sync(&[found(U1, "photos/photo-albums/1.html", album())])
            .unwrap();
        ix.db.pragma_update(None, "user_version", 7).unwrap();
        drop(ix);
        let ix = Index::open(&r.join("vk-saver")).unwrap();
        assert!(ix.rows(&[Status::New]).unwrap().is_empty());
        let v: i64 = ix
            .db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, VERSION);
    }

    #[test]
    fn corrupt_db_recreated() {
        let root = tmpdir("index-corrupt");
        let state = root.join("vk-saver");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("index.db"), vec![0xAB; 8192]).unwrap();
        let ix = Index::open(&state).unwrap();
        assert!(ix.rows(&[Status::New]).unwrap().is_empty());
        ix.db
            .query_row("SELECT count(*) FROM media", [], |r| r.get::<_, i64>(0))
            .unwrap();
    }

    #[test]
    fn sync_drops_unsaved_rows_absent_from_scan() {
        let (r, ix) = open("index-stale");
        let p = "photos/photo-albums/1.html";
        let (a, b, c) = (
            found(U1, p, album()),
            found(U2, "wall/w.html", Key::Unknown),
            found(U3, "wall/v.html", Key::Unknown),
        );
        ix.sync(&[a.clone(), b, c]).unwrap();
        save(&ix, U3, &saved_png(r.join("x")));
        ix.sync(&[a]).unwrap();
        let new: Vec<String> = ix
            .rows(&[Status::New])
            .unwrap()
            .into_iter()
            .map(|r| r.url)
            .collect();
        assert_eq!(new, [U1]);
        assert_eq!(row(&ix, U3).status, Status::Saved);
        assert_eq!(ix.counts().unwrap().values().sum::<u64>(), 2);
    }

    #[test]
    fn paths_survive_moved_root() {
        let (r, ix) = open("index-moved");
        ix.sync(&[found(U1, "photos/photo-albums/1.html", album())])
            .unwrap();
        save(&ix, U1, &saved_png(r.join("x")));
        let rel = ix.saved_paths().unwrap()[U1].clone();
        let f = safe_join(&r, &rel).unwrap();
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, b"x").unwrap();
        drop(ix);
        let moved = r.with_file_name(format!(
            "{}-moved",
            r.file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_dir_all(&moved);
        std::fs::rename(&r, &moved).unwrap();
        let ix = Index::open(&moved.join("vk-saver")).unwrap();
        let rel = &ix.saved_paths().unwrap()[U1];
        assert!(safe_join(&moved, rel).unwrap().is_file());
        assert_eq!(ix.reset_missing(&moved).unwrap(), 0);
    }
}
