use crate::cdn::{Cdn, CdnResponse, NetError};
use crate::clock::Clock;
use crate::download::FreeSpace;
use crate::progress::{Event, Progress};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub fn tmpdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vk-engine-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

#[derive(Clone, Default)]
pub struct SleepLog(Arc<Mutex<Vec<u64>>>);

impl SleepLog {
    pub fn sleeps(&self) -> Vec<u64> {
        self.0.lock().unwrap().clone()
    }
}

/// Clones share the time and the sleep log.
#[derive(Clone)]
pub struct FakeClock {
    now: Arc<Mutex<u64>>,
    sleeps: SleepLog,
}

impl FakeClock {
    pub fn new() -> (FakeClock, SleepLog) {
        let sleeps = SleepLog::default();
        let clock = FakeClock {
            now: Arc::new(Mutex::new(0)),
            sleeps: sleeps.clone(),
        };
        (clock, sleeps)
    }

    pub fn advance(&self, ms: u64) {
        *self.now.lock().unwrap() += ms;
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        *self.now.lock().unwrap()
    }
    fn sleep_ms(&self, ms: u64) {
        *self.now.lock().unwrap() += ms;
        self.sleeps.0.lock().unwrap().push(ms);
    }
}

pub struct NoProgress;

impl Progress for NoProgress {
    fn event(&self, _e: Event) {}
}

#[derive(Clone, Default)]
pub struct EventLog(Arc<Mutex<Vec<Event>>>);

impl EventLog {
    pub fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
}

impl Progress for EventLog {
    fn event(&self, e: Event) {
        self.0.lock().unwrap().push(e);
    }
}

pub struct FakeSpace(pub Result<u64, String>);

impl FreeSpace for FakeSpace {
    fn free_bytes(&self, _root: &Path) -> std::io::Result<u64> {
        self.0.clone().map_err(std::io::Error::other)
    }
}

/// One CDN reply. Status 200 serves `body[from..]` as 206 when `range` is on and `from > 0`.
#[derive(Clone)]
pub struct FakeFile {
    pub status: u16,
    pub body: Vec<u8>,
    /// Overrides the Content-Length of the served bytes.
    pub content_length: Option<u64>,
    pub range: bool,
    pub content_range: bool,
    /// Serves a 206 from this position instead of the requested one.
    pub range_start: Option<u64>,
    /// 416 when `from` is at or past the end of `body`.
    pub range_416: bool,
    /// The body read fails after the served bytes.
    pub broken: bool,
    /// `get` fails with this error instead of replying.
    pub net: Option<NetError>,
}

impl FakeFile {
    pub fn ok(body: &[u8]) -> FakeFile {
        FakeFile {
            status: 200,
            body: body.to_vec(),
            content_length: None,
            range: true,
            content_range: true,
            range_start: None,
            range_416: true,
            broken: false,
            net: None,
        }
    }

    pub fn net(e: NetError) -> FakeFile {
        FakeFile {
            net: Some(e),
            ..FakeFile::ok(b"")
        }
    }

    pub fn status(code: u16) -> FakeFile {
        FakeFile {
            status: code,
            ..FakeFile::ok(b"")
        }
    }
}

#[derive(Clone, Default)]
pub struct CdnLog(Arc<Mutex<Vec<(String, u64)>>>);

impl CdnLog {
    pub fn gets(&self) -> Vec<(String, u64)> {
        self.0.lock().unwrap().clone()
    }
}

/// Replies per url are consumed in order and the last one repeats; an unknown url panics.
pub struct FakeCdn {
    files: Mutex<HashMap<String, (Vec<FakeFile>, usize)>>,
    log: CdnLog,
}

impl FakeCdn {
    pub fn new(files: Vec<(&str, Vec<FakeFile>)>) -> (FakeCdn, CdnLog) {
        let log = CdnLog::default();
        let files = files
            .into_iter()
            .map(|(url, replies)| (url.to_string(), (replies, 0)))
            .collect();
        let cdn = FakeCdn {
            files: Mutex::new(files),
            log: log.clone(),
        };
        (cdn, log)
    }
}

struct BrokenRead;

impl Read for BrokenRead {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("connection reset"))
    }
}

impl Cdn for FakeCdn {
    fn get(&self, url: &str, from: u64) -> Result<CdnResponse, NetError> {
        self.log.0.lock().unwrap().push((url.to_string(), from));
        let mut files = self.files.lock().unwrap();
        let Some((replies, pos)) = files.get_mut(url) else {
            panic!("unexpected CDN url {url}");
        };
        let f = replies[(*pos).min(replies.len() - 1)].clone();
        *pos += 1;
        if let Some(e) = f.net {
            return Err(e);
        }
        let empty = |status| CdnResponse {
            status,
            content_length: Some(0),
            start: None,
            total: None,
            body: Box::new(std::io::empty()),
        };
        if f.status != 200 {
            return Ok(empty(f.status));
        }
        let len = f.body.len() as u64;
        let (status, start, total) = if from > 0 && f.range {
            if from >= len && f.range_416 {
                return Ok(empty(416));
            }
            (206, f.range_start.unwrap_or(from).min(len), Some(len))
        } else {
            (200, 0, None)
        };
        let shown = |v: u64| (status == 206 && f.content_range).then_some(v);
        let bytes = f.body[start as usize..].to_vec();
        let content_length = f.content_length.or(Some(bytes.len() as u64));
        let body: Box<dyn Read> = if f.broken {
            Box::new(std::io::Cursor::new(bytes).chain(BrokenRead))
        } else {
            Box::new(std::io::Cursor::new(bytes))
        };
        Ok(CdnResponse {
            status,
            content_length,
            start: shown(start),
            total: total.and_then(shown),
            body,
        })
    }
}

pub const PEER_A: &str = "2000000101";
pub const PEER_B: &str = "2000000102";
pub const PEER_USER: &str = "100002";
pub const PEER_CLUB: &str = "-200003";
pub const OWNER: &str = "200001";

pub const URL_ALBUM_1: &str = "https://sun9-1.userapi.com/impg/aA1bB2/5001.jpg?size=1280x960&quality=95&sign=0a1b5001&type=album";
pub const URL_ALBUM_2: &str = "https://sun9-2.userapi.com/impg/cC3dD4/5002.jpg?size=807x1080&quality=95&sign=0a1b5002&type=album";
pub const URL_ALBUM_3: &str = "https://sun9-3.userapi.com/impg/eE5fF6/5003.jpg?size=1080x1080&quality=96&sign=0a1b5003&type=album";
pub const URL_PLACEHOLDER: &str = "https://sun9-4.userapi.com/impg/deleted/placeholder.png?size=200x200&quality=95&sign=0a1b0000&type=album";
pub const URL_OLD_HOST: &str = "https://sun1-2.userapi.com/c1234/v1234/5a/x5007old.jpg";
pub const URL_CHAT_1: &str = "https://sun9-5.userapi.com/impg/gG7hH8/c1001.jpg?size=1280x720&quality=95&sign=0a1b1001&type=album";
pub const URL_CHAT_2: &str = "https://sun9-6.userapi.com/impg/iI9jJ0/c1002.jpg?size=960x1280&quality=95&sign=0a1b1002&type=album";
pub const URL_CHAT_3: &str = "https://sun1-3.userapi.com/c5678/v5678/1b/c1003old.jpg";
pub const URL_SHARED: &str = "https://sun9-7.userapi.com/impg/kK1lL2/shared.jpg?size=1024x768&quality=95&sign=0a1b2002&type=album";
pub const URL_VOICE: &str = "https://psv4-1.userapi.com/s/v1/d/aB3cD4eF/voice_1002.ogg";
pub const URL_AVATAR: &str = "https://sun9-8.userapi.com/impg/mM3nN4/avatar.jpg?size=400x400&quality=95&sign=0a1b3001&type=album";
pub const URL_WALL_DRIFT: &str = "https://sun9-9.userapi.com/impg/oO5pP6/wall.jpg?size=510x510&quality=95&sign=0a1b4001&type=album";

const URL_TAGGED_1: &str = "https://sun9-1.userapi.com/impg/aA1bB2/5001.jpg?size=604x453&quality=95&sign=0a1b6001&type=album";
const URL_HTTP: &str = "http://sun9-2.userapi.com/impg/qQ7rR8/plain.jpg?size=100x100&quality=95&sign=0a1b7001&type=album";
const URL_EVIL: &str = "https://sun1-1.userapi.com.evil.invalid/impg/sS9tT0/evil.jpg?size=100x100&quality=95&sign=0a1b8001&type=album";

#[derive(Default)]
pub struct FakeExportOpts {
    pub crlf: bool,
    pub utf8: bool,
}

fn esc(url: &str) -> String {
    url.replace('&', "&amp;")
}

fn page(charset: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"{charset}\">\n</head>\n<body>\n{body}</body>\n</html>\n"
    )
}

fn message(id: u32, from: &str, text: &str, attachments: &[(&str, &str)]) -> String {
    let mut out = format!(
        "<div class=\"item\"><div class='item__main'><div class=\"message\" data-id=\"{id}\"><div class=\"message__header\"><a href=\"https://vk.ru/id{from}\">Тестовый Пользователь</a>, 5 мар 2024 в 12:34:56</div><div>{text}<div class=\"kludges\">"
    );
    for (kind, href) in attachments {
        out += &format!(
            "<div class=\"attachment\"><div class=\"attachment__description\">{kind}</div><a class='attachment__link' href='{}'>{}</a></div>",
            esc(href),
            esc(href)
        );
    }
    out + "</div></div></div></div></div>\n"
}

fn album_photo(id: u32, url: &str) -> String {
    format!(
        "<div class=\"item\"><a href=\"https://vk.ru/photo{OWNER}_{id}\"><img src=\"{}\" alt=\"{OWNER}_{id}\"></a><div class=\"clear_fix\">5 мар 2024 в 12:34:56</div></div>\n",
        esc(url)
    )
}

/// A synthetic VK export in `tmpdir(name)`: cp1251 pages unless `opts.utf8`, LF unless `opts.crlf`.
pub fn fake_export(name: &str, opts: FakeExportOpts) -> PathBuf {
    let root = tmpdir(name);
    let charset = if opts.utf8 { "utf-8" } else { "windows-1251" };
    let vk = |p: &str| format!("https://vk.ru/{p}");
    let mut files: Vec<(String, String)> = Vec::new();

    files.push((
        "index.html".into(),
        page(
            charset,
            "<link rel=\"stylesheet\" href=\"style.css\">\n\
             <a class=\"menu__page-link\" href=\"messages/index-messages.html\">Сообщения</a>\n\
             <a class=\"menu__page-link\" href=\"photos/photo-albums/album-1.html\">Фотографии</a>\n\
             <a class=\"menu__page-link\" href=\"profile/page-info.html\">Профиль</a>\n\
             <a class=\"menu__page-link\" href=\"wall/wall0.html\">Стена</a>\n",
        ),
    ));

    let peers = [
        (PEER_A, "Тестовый чат"),
        (PEER_B, "Второй <b>&amp;</b> чат"),
        (PEER_USER, "Тестовый Пользователь"),
        (PEER_CLUB, "Тестовое сообщество"),
    ];
    let index: String = peers
        .iter()
        .map(|(peer, label)| {
            format!("<div class=\"item\"><div class=\"message-peer--id\"><a href=\"{peer}/messages0.html\">{label}</a></div></div>\n")
        })
        .collect();
    files.push(("messages/index-messages.html".into(), page(charset, &index)));

    let a0 = [
        message(1001, PEER_USER, "Привет", &[("Фотография", URL_CHAT_1)]),
        message(
            1002,
            OWNER,
            "Три вложения",
            &[
                ("Фотография", URL_CHAT_2),
                ("Фотография", URL_SHARED),
                ("Файл", URL_VOICE),
            ],
        ),
        message(
            1003,
            PEER_USER,
            "Старое фото",
            &[("Фотография", URL_CHAT_3)],
        ),
        message(1004, OWNER, "Документ", &[("Файл", &vk("doc200001_6001"))]),
        message(
            1005,
            PEER_USER,
            "Видео",
            &[("Видеозапись", &vk("video200001_7001"))],
        ),
        message(
            1006,
            OWNER,
            "Ссылка на фото",
            &[("Фотография", &vk("photo200001_5009"))],
        ),
        message(
            1007,
            PEER_USER,
            "Без шифрования",
            &[("Фотография", URL_HTTP)],
        ),
        message(1008, OWNER, "Чужой хост", &[("Фотография", URL_EVIL)]),
    ]
    .concat()
        + "<div class=\"pagination\"><a class=\"pg_lnk fl_l\" href=\"messages50.html\".html>2</a></div>\n";
    files.push((
        format!("messages/{PEER_A}/messages0.html"),
        page(charset, &a0),
    ));
    files.push((
        format!("messages/{PEER_A}/messages50.html"),
        page(
            charset,
            &message(
                1051,
                PEER_USER,
                "Вторая страница",
                &[("Фотография", URL_CHAT_1)],
            ),
        ),
    ));
    files.push((
        format!("messages/{PEER_B}/messages0.html"),
        page(
            charset,
            &message(1101, OWNER, "Общее фото", &[("Фотография", URL_SHARED)]),
        ),
    ));
    for peer in [PEER_USER, PEER_CLUB] {
        files.push((
            format!("messages/{peer}/messages0.html"),
            page(charset, &message(1201, peer, "Только текст", &[])),
        ));
    }

    let album = [
        album_photo(5001, URL_ALBUM_1),
        album_photo(5002, URL_ALBUM_2),
        album_photo(5003, URL_ALBUM_3),
        album_photo(5004, URL_PLACEHOLDER),
        album_photo(5005, URL_PLACEHOLDER),
        album_photo(5006, URL_PLACEHOLDER),
        album_photo(5007, URL_OLD_HOST),
    ]
    .concat();
    files.push((
        "photos/photo-albums/album-1.html".into(),
        page(charset, &album),
    ));
    files.push((
        "photos/photo-albums/tagged.html".into(),
        page(charset, &album_photo(5001, URL_TAGGED_1)),
    ));

    files.push((
        "profile/page-info.html".into(),
        page(
            charset,
            &format!(
                "<div class=\"profile\"><img src=\"{}\" alt=\"Тестовый Пользователь\"></div>\n",
                esc(URL_AVATAR)
            ),
        ),
    ));
    files.push((
        "profile/gifts.html".into(),
        page(charset, "<div class=\"item\"><img src=\"https://vk.ru/images/gift/1/256.jpg\" alt=\"Подарок\"></div>\n"),
    ));
    files.push((
        "wall/wall0.html".into(),
        page(
            charset,
            &format!(
                "<div class=\"wall_post\"><p>Запись</p><img class=\"post_img\" src=\"{}\"></div>\n",
                esc(URL_WALL_DRIFT)
            ),
        ),
    ));
    files.push((
        "style.css".into(),
        "body { font-family: sans-serif; }\n".into(),
    ));

    for (rel, text) in files {
        let text = if opts.crlf {
            text.replace('\n', "\r\n")
        } else {
            text
        };
        let bytes = if opts.utf8 {
            text.into_bytes()
        } else {
            crate::export::text::encode_cp1251(&text)
        };
        let path = rel.split('/').fold(root.clone(), |p, c| p.join(c));
        std::fs::create_dir_all(path.parent().unwrap()).expect("create fixture dir");
        std::fs::write(&path, bytes).expect("write fixture");
    }
    root
}
